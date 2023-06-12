package engine

import (
	"context"
	"errors"
	"fmt"
	"github.com/c2fo/vfs/v6"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/utils/instance"
	"github.com/hephbuild/heph/utils/xio"
	"github.com/hephbuild/heph/worker"
	"os"
	"path/filepath"
)

func ArtifactExternalFileName(a artifacts.Artifact) string {
	if a.Compressible() {
		return a.GzFileName()
	}

	return a.FileName()
}

func (e *Engine) localCacheLocation(target *Target) (vfs.Location, error) {
	// TODO: cache
	rel, err := filepath.Rel(e.LocalCache.Path.Abs(), e.cacheDir(target).Abs())
	if err != nil {
		return nil, err
	}

	return e.LocalCache.Location.NewLocation(rel + "/")
}

func (e *Engine) remoteCacheLocation(loc vfs.Location, target *Target) (vfs.Location, error) {
	// TODO: cache
	inputHash, err := e.LocalCache.hashInput(target, false)
	if err != nil {
		return nil, err
	}

	return loc.NewLocation(filepath.Join(target.Package.Path, target.Name, inputHash) + "/")
}

func (e *Engine) vfsCopyFileIfNotExists(ctx context.Context, from, to vfs.Location, path string, atomic bool) (bool, error) {
	tof, err := to.NewFile(path)
	if err != nil {
		return false, err
	}
	defer tof.Close()

	fromf, err := from.NewFile(path)
	if err != nil {
		return false, err
	}
	defer fromf.Close()

	exists, err := tof.Exists()
	if err != nil {
		return false, err
	}

	if exists {
		tos, _ := tof.Size()
		froms, _ := fromf.Size()
		if tos == froms {
			log.Tracef("vfs copy %v to %v: exists", from.URI(), to.URI())
			return false, nil
		}
	}

	_ = tof.Close()
	_ = fromf.Close()

	err = e.vfsCopyFile(ctx, from, to, path, atomic)
	if err != nil {
		return false, err
	}

	return true, nil
}

func (e *Engine) vfsCopyFile(ctx context.Context, from, to vfs.Location, path string, atomic bool) error {
	log.Tracef("vfs copy %v to %v", from.URI(), to.URI())

	doneTrace := log.TraceTimingDone(fmt.Sprintf("vfs copy to %v", to.URI()))
	defer doneTrace()

	sf, err := from.NewFile(path)
	if err != nil {
		return fmt.Errorf("NewFile sf: %w", err)
	}
	defer sf.Close()

	doneCloser := xio.CloserContext(sf, ctx)
	defer doneCloser()

	ok, err := sf.Exists()
	if err != nil {
		return fmt.Errorf("Exists: %w", err)
	}

	log.Tracef("%v exists: %v", sf.URI(), ok)

	if !ok {
		return fmt.Errorf("copy %v: %w", sf.URI(), os.ErrNotExist)
	}

	if atomic {

		dftmp, err := to.NewFile(path + "_tmp_" + instance.UID)
		if err != nil {
			return fmt.Errorf("NewFile df: %w", err)
		}
		defer dftmp.Close()

		err = sf.CopyToFile(dftmp)
		if err != nil {
			return err
		}

		df, err := to.NewFile(path)
		if err != nil {
			return fmt.Errorf("NewFile df: %w", err)
		}
		defer df.Close()

		err = dftmp.MoveToFile(df)
		if err != nil {
			return fmt.Errorf("Move: %w", err)
		}
	} else {
		df, err := to.NewFile(path)
		if err != nil {
			return fmt.Errorf("NewFile df: %w", err)
		}
		defer df.Close()

		err = sf.CopyToFile(df)
		if err != nil {
			return err
		}
	}

	return nil
}

func (e *Engine) scheduleStoreExternalCache(ctx context.Context, target *Target, cache graph.CacheConfig) *worker.Job {
	// input hash is used as a marker that everything went well,
	// wait for everything else to be done before copying the input hash
	inputHashArtifact := target.Artifacts.InputHash

	deps := &worker.WaitGroup{}
	for _, artifact := range target.Artifacts.All() {
		if artifact.Name() == inputHashArtifact.Name() {
			continue
		}

		j := e.scheduleStoreExternalCacheArtifact(ctx, target, cache, artifact, nil)
		deps.Add(j)
	}

	return e.scheduleStoreExternalCacheArtifact(ctx, target, cache, inputHashArtifact, deps)
}

func (e *Engine) scheduleStoreExternalCacheArtifact(ctx context.Context, target *Target, cache graph.CacheConfig, artifact artifacts.Artifact, deps *worker.WaitGroup) *worker.Job {
	return e.Pool.Schedule(ctx, &worker.Job{
		Name: fmt.Sprintf("cache %v %v %v", target.FQN, cache.Name, artifact.Name()),
		Deps: deps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			err := e.storeExternalCache(ctx, target, cache, artifact)
			if err != nil {
				return fmt.Errorf("store vfs cache %v: %v %w", cache.Name, target.FQN, err)
			}

			return nil
		},
	})
}

func (e *Engine) storeExternalCache(ctx context.Context, target *Target, cache graph.CacheConfig, artifact artifacts.Artifact) (rerr error) {
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

	status.Emit(ctx, TargetOutputStatus(target, artifact.DisplayName(), fmt.Sprintf("Uploading to %v...", cache.Name)))

	ctx, span := e.Observability.SpanCacheUpload(ctx, target.Target.Target, cache.Name, artifact)
	defer span.EndError(rerr)

	remoteRoot, err := e.remoteCacheLocation(cache.Location, target)
	if err != nil {
		return err
	}

	err = e.vfsCopyFile(ctx, localRoot, remoteRoot, ArtifactExternalFileName(artifact), false)
	if err != nil {
		return err
	}

	return nil
}

func (e *Engine) downloadExternalCache(ctx context.Context, target *Target, cache graph.CacheConfig, artifact artifacts.Artifact) (rerr error) {
	ctx, span := e.Observability.SpanCacheDownload(ctx, target.Target.Target, cache.Name, artifact)
	defer func() {
		if errors.Is(rerr, os.ErrNotExist) {
			span.SetCacheHit(false)
		}
		SpanEndNotExist(span, rerr)
	}()

	unlock, err := e.LocalCache.LockArtifact(ctx, target, artifact)
	if rerr != nil {
		return err
	}
	defer unlock()

	status.Emit(ctx, TargetOutputStatus(target, artifact.DisplayName(), fmt.Sprintf("Downloading from %v...", cache.Name)))

	localRoot, err := e.localCacheLocation(target)
	if err != nil {
		return err
	}

	remoteRoot, err := e.remoteCacheLocation(cache.Location, target)
	if err != nil {
		return err
	}

	copied, err := e.vfsCopyFileIfNotExists(ctx, remoteRoot, localRoot, ArtifactExternalFileName(artifact), true)
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

func (e *Engine) existsExternalCache(ctx context.Context, target *Target, cache graph.CacheConfig, artifact artifacts.Artifact) (bool, error) {
	status.Emit(ctx, TargetOutputStatus(target, artifact.DisplayName(), fmt.Sprintf("Checking from %v...", cache.Name)))

	root, err := e.remoteCacheLocation(cache.Location, target)
	if err != nil {
		return false, err
	}

	f, err := root.NewFile(ArtifactExternalFileName(artifact))
	if err != nil {
		return false, err
	}
	defer f.Close()

	return f.Exists()
}

func (e *Engine) existsLocalCache(ctx context.Context, target *Target, artifact artifacts.Artifact) (bool, error) {
	root, err := e.localCacheLocation(target)
	if err != nil {
		return false, err
	}

	for _, name := range []string{artifact.GzFileName(), artifact.FileName()} {
		f, err := root.NewFile(name)
		if err != nil {
			return false, err
		}

		exists, err := f.Exists()
		_ = f.Close()
		if err != nil {
			return false, err
		}

		if exists {
			return true, nil
		}
	}

	return false, nil
}
