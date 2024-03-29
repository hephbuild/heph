package rcache

import (
	"context"
	"errors"
	"fmt"
	"github.com/c2fo/vfs/v6"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/config"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/hroot"
	"github.com/hephbuild/heph/lcache"
	"github.com/hephbuild/heph/observability"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/locks"
	"github.com/hephbuild/heph/utils/xmath"
	"github.com/hephbuild/heph/vfssimple"
	"os"
	"path/filepath"
	"strings"
)

func SpanEndIgnoreNotExist(span observability.SpanError, err error) {
	if err == nil || errors.Is(err, os.ErrNotExist) {
		span.EndError(nil)
	} else {
		span.EndError(err)
	}
}

func artifactExternalFileName(a artifacts.Artifact) string {
	if a.Compressible() {
		return a.GzFileName()
	}

	return a.FileName()
}

type RemoteCache struct {
	Root          *hroot.State
	Config        *Config
	LocalCache    *lcache.LocalCacheState
	Observability *observability.Observability
	Hints         *HintStore

	orderedCachesLock locks.Locker
	orderedCaches     []CacheConfig
}

type Config struct {
	*config.Config
	Caches []CacheConfig
}

type CacheConfig struct {
	Name string
	config.Cache
	Location vfs.Location `yaml:"-"`
}

func New(root *hroot.State, cconfig *config.Config, localCache *lcache.LocalCacheState, observability *observability.Observability) (*RemoteCache, error) {
	cfg := &Config{Config: cconfig}

	for name, cache := range cconfig.Caches {
		uri := cache.URI
		if !strings.HasSuffix(uri, "/") {
			uri += "/"
		}
		loc, err := vfssimple.NewLocation(uri)
		if err != nil {
			return nil, fmt.Errorf("cache %v :%w", name, err)
		}

		cfg.Caches = append(cfg.Caches, CacheConfig{
			Name:     name,
			Cache:    cache,
			Location: loc,
		})
	}

	return &RemoteCache{
		Root:              root,
		Config:            cfg,
		LocalCache:        localCache,
		Observability:     observability,
		Hints:             &HintStore{},
		orderedCachesLock: locks.NewFlock("Order cache", root.Tmp.Join("order_cache.lock").Abs()),
	}, nil
}

func (e *RemoteCache) ArtifactExists(ctx context.Context, cache CacheConfig, target graph.Targeter, artifact artifacts.Artifact) (bool, error) {
	status.Emit(ctx, tgt.TargetOutputStatus(target, artifact.DisplayName(), fmt.Sprintf("Checking from %v...", cache.Name)))

	root, err := e.remoteCacheLocation(cache.Location, target)
	if err != nil {
		return false, err
	}

	f, err := root.NewFile(artifactExternalFileName(artifact))
	if err != nil {
		return false, err
	}
	defer f.Close()

	return f.Exists()
}

func (e *RemoteCache) DownloadArtifact(ctx context.Context, target graph.Targeter, cache CacheConfig, artifact artifacts.Artifact) (rerr error) {
	ctx, span := e.Observability.SpanCacheDownload(ctx, target.GraphTarget(), cache.Name, artifact)
	defer func() {
		if errors.Is(rerr, os.ErrNotExist) {
			span.SetCacheHit(false)
		}
		SpanEndIgnoreNotExist(span, rerr)
	}()

	status.Emit(ctx, tgt.TargetOutputStatus(target, artifact.DisplayName(), fmt.Sprintf("Downloading from %v...", cache.Name)))

	localRoot, err := e.LocalCache.VFSLocation(target)
	if err != nil {
		return err
	}

	remoteRoot, err := e.remoteCacheLocation(cache.Location, target)
	if err != nil {
		return err
	}

	// Optionally download manifest
	_, err = e.vfsCopyFileIfNotExists(ctx, remoteRoot, localRoot, artifact.ManifestFileName(), true, nil)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}

	copied, err := e.vfsCopyFileIfNotExists(ctx, remoteRoot, localRoot, artifactExternalFileName(artifact), true, func(percent float64) {
		status.EmitInteractive(ctx, tgt.TargetOutputStatus(target, artifact.DisplayName(),
			xmath.FormatPercent(fmt.Sprintf("Downloading from %v [P]...", cache.Name), percent)),
		)
	})
	if err != nil {
		return err
	}

	// A file may exist locally, but not remotely (coming from another source), make sure that it actually exists there
	if !copied {
		status.Emit(ctx, tgt.TargetOutputStatus(target, artifact.DisplayName(), fmt.Sprintf("Downloading from %v...", cache.Name)))

		remoteExist, err := e.ArtifactExists(ctx, cache, target, artifact)
		if err != nil {
			return err
		}

		if !remoteExist {
			return fmt.Errorf("%v: %w", filepath.Join(remoteRoot.URI(), artifact.Name()), os.ErrNotExist)
		}
	}

	span.SetCacheHit(true)

	return nil
}

func (e *RemoteCache) StoreArtifact(ctx context.Context, ttarget graph.Targeter, cache CacheConfig, artifact artifacts.Artifact) (rerr error) {
	target := ttarget.GraphTarget()

	status.Emit(ctx, tgt.TargetOutputStatus(target, artifact.DisplayName(), fmt.Sprintf("Uploading to %v...", cache.Name)))

	localRoot, err := e.LocalCache.VFSLocation(target)
	if err != nil {
		return err
	}

	ctx, span := e.Observability.SpanCacheUpload(ctx, target, cache.Name, artifact)
	defer span.EndError(rerr)

	remoteRoot, err := e.remoteCacheLocation(cache.Location, target)
	if err != nil {
		return err
	}

	err = e.vfsCopyFile(ctx, localRoot, remoteRoot, artifactExternalFileName(artifact), false, func(percent float64) {
		status.EmitInteractive(ctx, tgt.TargetOutputStatus(target, artifact.DisplayName(),
			xmath.FormatPercent(fmt.Sprintf("Uploading to %v [P]...", cache.Name), percent)),
		)
	})
	if err != nil {
		return err
	}

	status.Emit(ctx, tgt.TargetOutputStatus(target, artifact.DisplayName(), fmt.Sprintf("Uploading to %v...", cache.Name)))

	if artifact.GenerateManifest() {
		err = e.vfsCopyFile(ctx, localRoot, remoteRoot, artifact.ManifestFileName(), false, nil)
		if err != nil && errors.Is(err, os.ErrNotExist) {
			return err
		}
	}

	return nil
}

func (e *RemoteCache) WriteableCaches(ctx context.Context, starget specs.Specer) ([]CacheConfig, error) {
	target := starget.Spec()

	if !target.Cache.Enabled {
		return nil, nil
	}

	wcs := ads.Filter(e.Config.Caches, func(cache CacheConfig) bool {
		if !cache.Write {
			return false
		}

		if !target.Cache.NamedEnabled(cache.Name) {
			return false
		}

		return true
	})

	if len(wcs) == 0 {
		return nil, nil
	}

	orderedCaches, err := e.OrderedCaches(ctx)
	if err != nil {
		return nil, err
	}

	// Reset and re-add in order
	wcs = nil

	for _, cache := range orderedCaches {
		if !cache.Write {
			continue
		}

		if !target.Cache.NamedEnabled(cache.Name) {
			continue
		}

		wcs = append(wcs, cache)
	}

	return wcs, nil
}
