package engine

import (
	"context"
	"errors"
	"fmt"
	"github.com/c2fo/vfs/v6"
	"heph/engine/artifacts"
	log "heph/hlog"
	"heph/utils"
	"heph/worker"
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

func (e *Engine) vfsCopyFileIfNotExists(ctx context.Context, from, to vfs.Location, path string) error {
	tof, err := to.NewFile(path)
	if err != nil {
		return err
	}

	exists, err := tof.Exists()
	_ = tof.Close()
	if err != nil {
		return err
	}

	if exists {
		log.Tracef("vfs copy %v to %v: exists", from.URI(), to.URI())
		return nil
	}

	return e.vfsCopyFile(ctx, from, to, path)
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
		return fmt.Errorf("vfs %v: %w", sf.URI(), os.ErrNotExist)
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

func (e *TargetRunEngine) storeExternalCache(ctx context.Context, target *Target, cache CacheConfig, artifact artifacts.Artifact) error {
	e.Status(TargetOutputStatus(target, artifact.DisplayName(), fmt.Sprintf("Uploading to %v...", cache.Name)))

	localRoot, err := e.localCacheLocation(target)
	if err != nil {
		return err
	}

	remoteRoot, err := e.remoteCacheLocation(cache.Location, target)
	if err != nil {
		return err
	}

	err = e.vfsCopyFile(ctx, localRoot, remoteRoot, artifact.Name())
	if err != nil {
		if !artifact.GenRequired() && errors.Is(err, os.ErrNotExist) {
			return nil
		}

		return err
	}

	return nil
}

func (e *TargetRunEngine) downloadExternalCache(ctx context.Context, target *Target, cache CacheConfig, artifact artifacts.Artifact) error {
	e.Status(TargetOutputStatus(target, artifact.DisplayName(), fmt.Sprintf("Downloading from %v...", cache.Name)))

	localRoot, err := e.localCacheLocation(target)
	if err != nil {
		return err
	}

	remoteRoot, err := e.remoteCacheLocation(cache.Location, target)
	if err != nil {
		return err
	}

	return e.vfsCopyFileIfNotExists(ctx, remoteRoot, localRoot, artifact.Name())
}
