package engine

import (
	"context"
	"errors"
	"fmt"
	"github.com/c2fo/vfs/v6"
	"github.com/hephbuild/heph/engine/artifacts"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils"
	"github.com/hephbuild/heph/worker"
	"os"
	"path/filepath"
)

func (e *Engine) localCacheLocation(target *Target) (vfs.Location, error) {
	// TODO: cache
	rel, err := filepath.Rel(e.LocalCache.Path(), e.cacheDir(target).Abs())
	if err != nil {
		return nil, err
	}

	return e.LocalCache.NewLocation(rel + "/")
}

func (e *Engine) remoteCacheLocation(loc vfs.Location, target *Target) (vfs.Location, error) {
	// TODO: cache
	inputHash := e.hashInput(target)

	return loc.NewLocation(filepath.Join(target.Package.FullName, target.Name, inputHash) + "/")
}

func (e *Engine) vfsCopyFileIfNotExists(ctx context.Context, from, to vfs.Location, path string) (bool, error) {
	tof, err := to.NewFile(path)
	if err != nil {
		return false, err
	}

	exists, err := tof.Exists()
	_ = tof.Close()
	if err != nil {
		return false, err
	}

	if exists {
		log.Tracef("vfs copy %v to %v: exists", from.URI(), to.URI())
		return false, nil
	}

	err = e.vfsCopyFile(ctx, from, to, path)
	if err != nil {
		return false, err
	}

	return true, nil
}

func (e *Engine) vfsCopyFile(ctx context.Context, from, to vfs.Location, path string) error {
	log.Tracef("vfs copy %v to %v", from.URI(), to.URI())

	doneTrace := utils.TraceTimingDone(fmt.Sprintf("vfs copy to %v", to.URI()))
	defer doneTrace()

	sf, err := from.NewFile(path)
	if err != nil {
		return fmt.Errorf("NewFile: %w", err)
	}
	defer sf.Close()

	doneCloser := utils.CloserContext(sf, ctx)
	defer doneCloser()

	ok, err := sf.Exists()
	if err != nil {
		return fmt.Errorf("Exists: %w", err)
	}

	log.Tracef("%v exists: %v", sf.URI(), ok)

	if !ok {
		return fmt.Errorf("copy %v: %w", sf.URI(), os.ErrNotExist)
	}

	df, err := sf.CopyToLocation(to)
	if err != nil {
		return fmt.Errorf("CopyToLocation: %w", err)
	}
	defer df.Close()

	return nil
}

func (e *TargetRunEngine) scheduleStoreExternalCache(ctx context.Context, target *Target, cache CacheConfig) *worker.Job {
	// input hash is used as a marker that everything went well,
	// wait for everything else to be done before copying the input hash
	inputHashArtifact := target.artifacts.InputHash

	deps := &worker.WaitGroup{}
	for _, artifact := range target.artifacts.All() {
		if artifact.Name() == inputHashArtifact.Name() {
			continue
		}

		j := e.scheduleStoreExternalCacheArtifact(ctx, target, cache, artifact, nil)
		deps.Add(j)
	}

	return e.scheduleStoreExternalCacheArtifact(ctx, target, cache, inputHashArtifact, deps)
}

func (e *TargetRunEngine) scheduleStoreExternalCacheArtifact(ctx context.Context, target *Target, cache CacheConfig, artifact artifacts.Artifact, deps *worker.WaitGroup) *worker.Job {
	return e.Pool.Schedule(ctx, &worker.Job{
		Name: fmt.Sprintf("cache %v %v %v", target.FQN, cache.Name, artifact.Name()),
		Deps: deps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			e := NewTargetRunEngine(e.Engine, w.Status)

			err := e.storeExternalCache(ctx, target, cache, artifact)
			if err != nil {
				return fmt.Errorf("store vfs cache %v: %v %w", cache.Name, target.FQN, err)
			}

			return nil
		},
	})
}

func (e *TargetRunEngine) storeExternalCache(ctx context.Context, target *Target, cache CacheConfig, artifact artifacts.Artifact) (rerr error) {
	localRoot, err := e.localCacheLocation(target)
	if err != nil {
		return err
	}

	exists, err := e.existsLocalCache(ctx, target, artifact)
	if err != nil {
		return err
	}

	if !exists {
		if artifact.GenRequired() {
			return fmt.Errorf("%v: %v is supposed to exist but doesn't", target.FQN, artifact.Name())
		}

		return nil
	}

	e.Status(TargetOutputStatus(target, artifact.DisplayName(), fmt.Sprintf("Uploading to %v...", cache.Name)))

	ctx, span := e.Observability.SpanCacheUpload(ctx, target.Target, cache.Name, artifact)
	defer span.EndError(rerr)

	remoteRoot, err := e.remoteCacheLocation(cache.Location, target)
	if err != nil {
		return err
	}

	err = e.vfsCopyFile(ctx, localRoot, remoteRoot, artifact.Name())
	if err != nil {
		return err
	}

	return nil
}

func (e *TargetRunEngine) downloadExternalCache(ctx context.Context, target *Target, cache CacheConfig, artifact artifacts.Artifact) (rerr error) {
	ctx, span := e.Observability.SpanCacheDownload(ctx, target.Target, cache.Name, artifact)
	defer func() {
		if rerr != nil && errors.Is(rerr, os.ErrNotExist) {
			span.SetCacheHit(false)
		}
		span.EndError(rerr)
	}()

	e.Status(TargetOutputStatus(target, artifact.DisplayName(), fmt.Sprintf("Downloading from %v...", cache.Name)))

	err := target.cacheLocks[artifact.Name()].Lock(ctx)
	if err != nil {
		return err
	}
	defer func() {
		err := target.cacheLocks[artifact.Name()].Unlock()
		if err != nil {
			log.Errorf("unlock %v %v: %v", target.FQN, artifact.Name(), err)
		}
	}()

	localRoot, err := e.localCacheLocation(target)
	if err != nil {
		return err
	}

	remoteRoot, err := e.remoteCacheLocation(cache.Location, target)
	if err != nil {
		return err
	}

	copied, err := e.vfsCopyFileIfNotExists(ctx, remoteRoot, localRoot, artifact.Name())
	if err != nil {
		return err
	}

	// A file may exist locally, but not remotely (coming from another source), make sure that it actually exists there
	if !copied {
		remoteExist, err := e.existsExternalCache(ctx, target, cache, artifact)
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

func (e *TargetRunEngine) existsExternalCache(ctx context.Context, target *Target, cache CacheConfig, artifact artifacts.Artifact) (bool, error) {
	e.Status(TargetOutputStatus(target, artifact.DisplayName(), fmt.Sprintf("Checking from %v...", cache.Name)))

	root, err := e.remoteCacheLocation(cache.Location, target)
	if err != nil {
		return false, err
	}

	f, err := root.NewFile(artifact.Name())
	if err != nil {
		return false, err
	}
	defer f.Close()

	return f.Exists()
}

func (e *TargetRunEngine) existsLocalCache(ctx context.Context, target *Target, artifact artifacts.Artifact) (bool, error) {
	root, err := e.localCacheLocation(target)
	if err != nil {
		return false, err
	}

	f, err := root.NewFile(artifact.Name())
	if err != nil {
		return false, err
	}
	defer f.Close()

	return f.Exists()
}
