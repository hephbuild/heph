package engine

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"path"
	"sync"

	"github.com/hephbuild/heph/internal/herrgroup"

	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hinstance"
	"github.com/hephbuild/heph/internal/hrand"
	"github.com/hephbuild/heph/internal/hslices"
	"github.com/hephbuild/heph/lib/pluginsdk"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/trace"
)

func (e *Engine) CacheRemotely(ctx context.Context, def *LightLinkedTarget, hashin string, manifest *hartifact.Manifest, artifacts []*ResultArtifact) {
	if def.GetDisableRemoteCache() {
		return
	}

	var hasWriteCache bool
	for _, cache := range e.Caches {
		if cache.Write {
			hasWriteCache = true
			break
		}
	}

	if !hasWriteCache {
		return
	}

	var g herrgroup.Group
	step, ctx := hstep.New(ctx, "Uploading to remote cache...")
	defer step.Done()

	for _, cache := range e.Caches {
		if !cache.Write {
			continue
		}

		g.Go(func() error {
			err := e.cacheRemotelyInner(ctx, def.GetRef(), hashin, manifest, artifacts, cache)
			if err != nil {
				hlog.From(ctx).With(slog.String("cache", cache.Name), slog.String("err", err.Error())).Error("failed to cache remotely")
			}
			return nil
		})
	}

	err := g.Wait()
	if err != nil {
		hlog.From(ctx).With(slog.String("err", err.Error())).Error("failed to cache remotely")
	}
}

func (e *Engine) cacheRemotelyInner(ctx context.Context,
	ref *pluginv1.TargetRef,
	hashin string,
	manifest *hartifact.Manifest,
	artifacts []*ResultArtifact,
	cache CacheHandle,
) error {
	step, ctx := hstep.New(ctx, fmt.Sprintf("Caching %q...", cache.Name))
	defer step.Done()

	// TODO: remote lock ?

	for _, artifact := range artifacts {
		r, err := hartifact.Reader(artifact.Artifact)
		if err != nil {
			return err
		}
		defer r.Close()

		artifactName := artifact.GetName()
		if artifact.GetType() == pluginv1.Artifact_TYPE_MANIFEST_V1 {
			artifactName = hartifact.ManifestName
		}

		key := e.remoteCacheKey(ref, hashin, artifactName)

		err = cache.Client.Store(ctx, key, r)
		if err != nil {
			return err
		}

		_ = r.Close()
	}

	return nil
}

func (e *Engine) ResultFromRemoteCache(ctx context.Context, rs *RequestState, def *LightLinkedTarget, outputs []string, hashin string) (*ExecuteResult, bool, error) {
	ref := def.GetRef()

	if def.GetDisableRemoteCache() {
		return nil, false, nil
	}

	var hasReadCache bool
	for _, cache := range e.Caches {
		if cache.Read {
			hasReadCache = true
			break
		}
	}

	if !hasReadCache {
		return nil, false, nil
	}

	ctx, span := tracer.Start(ctx, "ResultFromRemoteCache")
	defer span.End()

	step, ctx := hstep.New(ctx, "Pulling from remote cache...")
	defer step.Done()

	for _, cache := range e.Caches {
		if !cache.Read {
			continue
		}

		tmpCacheDir := hfs.At(
			e.Cache,
			def.GetRef().GetPackage(),
			e.targetDirName(def.GetRef())+"_remote_tmp_"+hinstance.UID+"_"+hrand.Str(7)+"_"+hashin,
		)
		err := tmpCacheDir.MkdirAll(os.ModePerm)
		if err != nil {
			return nil, false, err
		}

		defer tmpCacheDir.RemoveAll()

		cacheDir := hfs.At(e.Cache, def.GetRef().GetPackage(), e.targetDirName(def.GetRef()), hashin)

		artifacts, ok, err := e.resultFromRemoteCacheInner(ctx, ref, outputs, hashin, cache, tmpCacheDir)
		if err != nil {
			hlog.From(ctx).With(slog.String("cache", cache.Name), slog.String("err", err.Error())).Error("failed to get from cache")
			continue
		}

		if ok {
			localArtifacts := make([]*ResultArtifact, 0, len(artifacts))
			for _, artifact := range artifacts {
				to := cacheDir.At(artifact.GetName())

				err = hfs.Move(tmpCacheDir.At(artifact.GetName()), to)
				if err != nil {
					return nil, false, err
				}

				rartifact, err := hartifact.Relocated(artifact.GetProto(), to.Path())
				if err != nil {
					return nil, false, err
				}

				localArtifacts = append(localArtifacts, &ResultArtifact{
					Hashout:  artifact.Hashout,
					Artifact: protoPluginArtifact{Artifact: rartifact},
				})
			}

			return ExecuteResult{
				Def:       def,
				Executed:  false,
				Hashin:    hashin,
				Artifacts: localArtifacts,
			}.Sorted(), true, nil
		}

		_ = tmpCacheDir.RemoveAll()
	}

	return nil, false, nil
}

func (e *Engine) manifestFromRemoteCache(ctx context.Context, ref *pluginv1.TargetRef, hashin string, cache CacheHandle) (hartifact.Manifest, bool, error) {
	manifestKey := e.remoteCacheKey(ref, hashin, hartifact.ManifestName)

	r, _, err := cache.Client.Get(ctx, manifestKey)
	if err != nil {
		if errors.Is(err, pluginsdk.ErrCacheNotFound) {
			return hartifact.Manifest{}, false, nil
		}

		return hartifact.Manifest{}, false, nil
	}
	defer r.Close()

	m, err := hartifact.DecodeManifest(r)
	if err != nil {
		if errors.Is(err, pluginsdk.ErrCacheNotFound) {
			return hartifact.Manifest{}, false, nil
		}

		return hartifact.Manifest{}, false, err
	}

	return m, true, nil
}

func (e *Engine) remoteCacheKey(ref *pluginv1.TargetRef, hashin, artifactName string) string {
	targetDirName := e.targetDirName(ref)

	return path.Join(ref.GetPackage(), targetDirName, hashin, artifactName)
}

func (e *Engine) resultFromRemoteCacheInner(
	ctx context.Context,
	ref *pluginv1.TargetRef,
	outputs []string,
	hashin string,
	cache CacheHandle,
	cachedir hfs.OS,
) ([]*ResultArtifact, bool, error) {
	ctx, span := tracer.Start(ctx, "ResultFromLocalCacheInner", trace.WithAttributes(attribute.String("cache", cache.Name)))
	defer span.End()

	manifest, ok, err := e.manifestFromRemoteCache(ctx, ref, hashin, cache)
	if err != nil {
		return nil, false, err
	}
	if !ok {
		return nil, false, nil
	}

	remoteArtifacts := make([]hartifact.ManifestArtifact, 0, len(outputs))
	for _, output := range outputs {
		artifact, ok := hslices.Find(manifest.Artifacts, func(artifact hartifact.ManifestArtifact) bool {
			if artifact.Type != hartifact.ManifestArtifactType(pluginv1.Artifact_TYPE_OUTPUT) {
				return false
			}

			return artifact.Group == output
		})
		if !ok {
			return nil, false, nil
		}

		remoteArtifacts = append(remoteArtifacts, artifact)
	}

	var localArtifactsm sync.Mutex
	localArtifacts := make([]*ResultArtifact, 0, len(outputs))

	g := herrgroup.NewContext(ctx, true)

	for _, artifact := range remoteArtifacts {
		key := e.remoteCacheKey(ref, hashin, artifact.Name)

		g.Go(func(ctx context.Context) error {
			r, size, err := cache.Client.Get(ctx, key)
			if err != nil {
				if errors.Is(err, pluginsdk.ErrCacheNotFound) {
					return pluginsdk.ErrCacheNotFound
				}

				return err
			}
			defer r.Close()

			localArtifact, err := e.cacheLocally(ctx, ref, hashin, CacheLocallyArtifact{
				Reader: r,
				Size:   size,
				Type:   pluginv1.Artifact_Type(artifact.Type),
				Group:  artifact.Group,
				Name:   artifact.Name,
			}, artifact.Hashout)
			if err != nil {
				return err
			}

			localArtifactsm.Lock()
			localArtifacts = append(localArtifacts, localArtifact)
			localArtifactsm.Unlock()

			return nil
		})
	}
	err = g.Wait()
	if err != nil {
		if errors.Is(err, pluginsdk.ErrCacheNotFound) {
			return nil, false, nil
		}

		return nil, false, err
	}

	localArtifacts = append(localArtifacts, &ResultArtifact{
		Artifact: &manifestPluginArtifact{manifest: manifest},
	})

	return localArtifacts, true, nil
}
