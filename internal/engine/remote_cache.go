package engine

import (
	"context"
	"errors"
	"github.com/hephbuild/heph/herrgroup"
	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hcore/hstep"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hinstance"
	"github.com/hephbuild/heph/internal/hrand"
	"github.com/hephbuild/heph/internal/hslices"
	engine2 "github.com/hephbuild/heph/lib/engine"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/trace"
	"golang.org/x/sync/errgroup"
	"io"
	"log/slog"
	"os"
	"path"
	"sync"
)

func (e *Engine) CacheRemotely(ctx context.Context, def *LightLinkedTarget, hashin string, manifest *hartifact.Manifest, artifacts []ExecuteResultArtifact) {
	if def.DisableRemoteCache {
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

func (e *Engine) cacheRemotelyInner(ctx context.Context, ref *pluginv1.TargetRef, hashin string, manifest *hartifact.Manifest, artifacts []ExecuteResultArtifact, cache CacheHandle) error {
	// TODO: remote lock ?

	for _, artifact := range artifacts {
		r, err := hartifact.Reader(ctx, artifact.Artifact)
		if err != nil {
			return err
		}
		defer r.Close()

		key := e.remoteCacheKey(ref, hashin, artifact.Name)

		err = cache.Client.Store(ctx, key, r)
		if err != nil {
			return err
		}

		_ = r.Close()
	}

	return nil
}

func (e *Engine) ResultFromRemoteCache(ctx context.Context, def *LightLinkedTarget, outputs []string, hashin string, rs *RequestState) (*ExecuteResult, bool, error) {
	ref := def.GetRef()

	if def.DisableRemoteCache {
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

		tmpCacheDir := hfs.At(e.Cache, def.GetRef().GetPackage(), e.targetDirName(def.GetRef())+"_remote_tmp_"+hinstance.UID+"_"+hrand.Str(7)+"_"+hashin)
		err := tmpCacheDir.MkdirAll("", os.ModePerm)
		if err != nil {
			return nil, false, err
		}

		defer tmpCacheDir.RemoveAll("")

		cacheDir := hfs.At(e.Cache, def.GetRef().GetPackage(), e.targetDirName(def.GetRef()), hashin)

		artifacts, ok, err := e.resultFromRemoteCacheInner(ctx, ref, outputs, hashin, cache, tmpCacheDir)
		if err != nil {
			hlog.From(ctx).With(slog.String("cache", cache.Name), slog.String("err", err.Error())).Error("failed to get from cache")
			continue
		}

		if ok {
			localArtifacts := make([]ExecuteResultArtifact, 0, len(artifacts))
			for _, artifact := range artifacts {
				to := cacheDir.At(artifact.Name)

				err = hfs.Move(tmpCacheDir.At(artifact.Name), to)
				if err != nil {
					return nil, false, err
				}

				rartifact, err := hartifact.Relocated(artifact.Artifact, to.Path())
				if err != nil {
					return nil, false, err
				}

				localArtifacts = append(localArtifacts, ExecuteResultArtifact{
					Hashout:  artifact.Hashout,
					Artifact: rartifact,
				})
			}

			return ExecuteResult{
				Def:       def,
				Executed:  false,
				Hashin:    hashin,
				Artifacts: localArtifacts,
			}.Sorted(), true, nil
		}

		_ = tmpCacheDir.RemoveAll("")
	}

	return nil, false, nil
}

func (e *Engine) manifestFromRemoteCache(ctx context.Context, ref *pluginv1.TargetRef, hashin string, cache CacheHandle) (hartifact.Manifest, bool, error) {
	targetDirName := e.targetDirName(ref)

	manifestKey := path.Join(ref.GetPackage(), targetDirName, hashin, hartifact.ManifestName)

	r, err := cache.Client.Get(ctx, manifestKey)
	if err != nil {
		if errors.Is(err, engine2.ErrCacheNotFound) {
			return hartifact.Manifest{}, false, nil
		}

		return hartifact.Manifest{}, false, nil
	}
	defer r.Close()

	m, err := hartifact.DecodeManifest(r)
	if err != nil {
		if errors.Is(err, engine2.ErrCacheNotFound) {
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

func (e *Engine) resultFromRemoteCacheInner(ctx context.Context, ref *pluginv1.TargetRef, outputs []string, hashin string, cache CacheHandle, cachedir hfs.OS) ([]ExecuteResultArtifact, bool, error) {
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
	localArtifacts := make([]ExecuteResultArtifact, 0, len(outputs))

	g, ctx := errgroup.WithContext(ctx)

	for _, artifact := range remoteArtifacts {
		key := e.remoteCacheKey(ref, hashin, artifact.Name)

		tofs := cachedir.At(artifact.Name)

		g.Go(func() error {
			r, err := cache.Client.Get(ctx, key)
			if err != nil {
				if errors.Is(err, engine2.ErrCacheNotFound) {
					return engine2.ErrCacheNotFound
				}

				return err
			}

			f, err := hfs.Create(tofs, "")
			if err != nil {
				return err
			}
			defer f.Close()

			_, err = io.Copy(f, r)
			if err != nil {
				return err
			}
			_ = f.Close()

			lartifact, err := hartifact.ManifestArtifactToProto(artifact, tofs.Path())
			if err != nil {
				return err
			}

			localArtifactsm.Lock()
			localArtifacts = append(localArtifacts, ExecuteResultArtifact{
				Hashout:  artifact.Hashout,
				Artifact: lartifact,
			})
			localArtifactsm.Unlock()

			return nil
		})
	}
	err = g.Wait()
	if err != nil {
		if errors.Is(err, engine2.ErrCacheNotFound) {
			return nil, false, nil
		}

		return nil, false, err
	}

	manifestArtifact, err := hartifact.WriteManifest(cachedir, manifest)
	if err != nil {
		return nil, false, err
	}

	localArtifacts = append(localArtifacts, ExecuteResultArtifact{
		Artifact: manifestArtifact,
	})

	return localArtifacts, true, nil
}
