package engine

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"path"

	"github.com/hephbuild/heph/internal/herrgroup"

	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/hcore/hstep"
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

	g := herrgroup.NewContext(ctx, true)

	for _, artifact := range artifacts {
		g.Go(func(ctx context.Context) error {
			r, err := artifact.GetContentReader()
			if err != nil {
				return err
			}
			defer r.Close()

			key := e.remoteCacheKey(ref, hashin, artifact.GetName())

			err = cache.Client.Store(ctx, key, r)
			if err != nil {
				return err
			}

			return nil
		})
	}

	key := e.remoteCacheKey(ref, hashin, hartifact.ManifestName)

	pr, pw := io.Pipe()

	go func() {
		defer pw.Close()
		err := hartifact.EncodeManifest(pw, manifest)
		if err != nil {
			_ = pw.CloseWithError(err)

			return
		}
	}()

	err := cache.Client.Store(ctx, key, pr)
	if err != nil {
		return err
	}

	return nil
}

func (e *Engine) ResultFromRemoteCache(ctx context.Context, rs *RequestState, def *LightLinkedTarget, outputs []string, hashin string) (*Result, bool, error) {
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

		ok, err := e.resultFromRemoteCacheInner(ctx, ref, outputs, hashin, cache)
		if err != nil {
			hlog.From(ctx).With(slog.String("cache", cache.Name), slog.String("err", err.Error())).Error("failed to get from cache")
			continue
		}
		if !ok {
			continue
		}

		res, ok, err := e.ResultFromLocalCache(ctx, def, outputs, hashin)
		if err != nil {
			// this is really not supposed to happen...

			return nil, false, fmt.Errorf("malformed local cache from remote cache: %w", err)
		}

		if ok {
			return res, true, nil
		}
	}

	return nil, false, nil
}

func (e *Engine) manifestFromRemoteCache(ctx context.Context, ref *pluginv1.TargetRef, hashin string, cache CacheHandle) (*hartifact.Manifest, bool, error) {
	manifestKey := e.remoteCacheKey(ref, hashin, hartifact.ManifestName)

	r, err := cache.Client.Get(ctx, manifestKey)
	if err != nil {
		if errors.Is(err, pluginsdk.ErrCacheNotFound) {
			return nil, false, nil
		}

		return nil, false, nil
	}
	defer r.Close()

	m, err := hartifact.DecodeManifest(r)
	if err != nil {
		if errors.Is(err, pluginsdk.ErrCacheNotFound) {
			return nil, false, nil
		}

		return nil, false, err
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
) (bool, error) {
	ctx, span := tracer.Start(ctx, "ResultFromLocalCacheInner", trace.WithAttributes(attribute.String("cache", cache.Name)))
	defer span.End()

	manifest, ok, err := e.manifestFromRemoteCache(ctx, ref, hashin, cache)
	if err != nil {
		return false, err
	}
	if !ok {
		return false, nil
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
			return false, nil
		}

		remoteArtifacts = append(remoteArtifacts, artifact)
	}

	localArtifacts := make([]*ResultArtifact, len(outputs))

	g := herrgroup.NewContext(ctx, true)

	for i, artifact := range remoteArtifacts {
		key := e.remoteCacheKey(ref, hashin, artifact.Name)

		g.Go(func(ctx context.Context) error {
			r, err := cache.Client.Get(ctx, key)
			if err != nil {
				return err
			}
			defer r.Close()

			localArtifact, err := e.cacheArtifactLocally(ctx, ref, hashin, CacheLocallyArtifact{
				Reader:      r,
				Size:        artifact.Size,
				Type:        pluginv1.Artifact_Type(artifact.Type),
				Group:       artifact.Group,
				Name:        artifact.Name,
				ContentType: artifact.ContentType,
			}, artifact.Hashout)
			if err != nil {
				return err
			}

			localArtifacts[i] = localArtifact

			return nil
		})
	}
	err = g.Wait()
	if err != nil {
		if errors.Is(err, pluginsdk.ErrCacheNotFound) {
			return false, nil
		}

		return false, err
	}

	_, err = e.createLocalCacheManifest(ctx, ref, hashin, localArtifacts)
	if err != nil {
		return false, err
	}

	return true, nil
}
