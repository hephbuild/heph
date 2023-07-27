package rcache

import (
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/lcache"
	"github.com/hephbuild/heph/observability"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/tgt"
	"os"
	"path/filepath"
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
	LocalCache    *lcache.LocalCacheState
	Observability *observability.Observability
}

func New(localCache *lcache.LocalCacheState, observability *observability.Observability) *RemoteCache {
	return &RemoteCache{
		LocalCache:    localCache,
		Observability: observability,
	}
}

func (e *RemoteCache) ArtifactExists(ctx context.Context, cache graph.CacheConfig, target graph.Targeter, artifact artifacts.Artifact) (bool, error) {
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

func (e *RemoteCache) DownloadArtifact(ctx context.Context, target graph.Targeter, cache graph.CacheConfig, artifact artifacts.Artifact) (rerr error) {
	ctx, span := e.Observability.SpanCacheDownload(ctx, target.TGTTarget(), cache.Name, artifact)
	defer func() {
		if errors.Is(rerr, os.ErrNotExist) {
			span.SetCacheHit(false)
		}
		SpanEndIgnoreNotExist(span, rerr)
	}()

	unlock, err := e.LocalCache.LockArtifact(ctx, target, artifact)
	if err != nil {
		return err
	}
	defer unlock()

	status.Emit(ctx, tgt.TargetOutputStatus(target, artifact.DisplayName(), fmt.Sprintf("Downloading from %v...", cache.Name)))

	localRoot, err := e.LocalCache.VFSLocation(target)
	if err != nil {
		return err
	}

	remoteRoot, err := e.remoteCacheLocation(cache.Location, target)
	if err != nil {
		return err
	}

	copied, err := e.vfsCopyFileIfNotExists(ctx, remoteRoot, localRoot, artifactExternalFileName(artifact), true)
	if err != nil {
		return err
	}

	// A file may exist locally, but not remotely (coming from another source), make sure that it actually exists there
	if !copied {
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

func (e *RemoteCache) StoreArtifact(ctx context.Context, ttarget graph.Targeter, cache graph.CacheConfig, artifact artifacts.Artifact) (rerr error) {
	target := ttarget.GraphTarget()

	localRoot, err := e.LocalCache.VFSLocation(target)
	if err != nil {
		return err
	}

	exists, err := e.LocalCache.Exists(ctx, target, artifact)
	if err != nil {
		return err
	}

	if !exists {
		if !artifact.GenRequired() {
			return nil
		}

		return fmt.Errorf("%v: %v is supposed to exist but doesn't", target.FQN, artifact.Name())
	}

	status.Emit(ctx, tgt.TargetOutputStatus(target, artifact.DisplayName(), fmt.Sprintf("Uploading to %v...", cache.Name)))

	ctx, span := e.Observability.SpanCacheUpload(ctx, target.Target, cache.Name, artifact)
	defer span.EndError(rerr)

	remoteRoot, err := e.remoteCacheLocation(cache.Location, target)
	if err != nil {
		return err
	}

	err = e.vfsCopyFile(ctx, localRoot, remoteRoot, artifactExternalFileName(artifact), false)
	if err != nil {
		return err
	}

	return nil
}
