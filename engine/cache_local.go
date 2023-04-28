package engine

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/engine/artifacts"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils/fs"
	"os"
)

func (e *TargetRunEngine) storeCache(ctx context.Context, target *Target, outRoot string, logFilePath string) (rerr error) {
	if target.ConcurrentExecution {
		log.Debugf("%v concurrent execution, skipping storeCache", target.FQN)
		return nil
	}

	if target.Cache.Enabled {
		e.Status(TargetStatus(target, "Caching..."))
	} else if len(target.artifacts.Out) > 0 {
		e.Status(TargetStatus(target, "Storing output..."))
	}

	// Calling AllStore here since the InputHash must be the last one stored,
	// its used as a marker that everything went well
	allArtifacts := target.artifacts.AllStore()

	for _, artifact := range allArtifacts {
		err := target.cacheLocks[artifact.Name()].Lock(ctx)
		if err != nil {
			return fmt.Errorf("lock %v %v: %w", target.FQN, artifact.Name(), err)
		}
	}
	defer func() {
		for _, artifact := range allArtifacts {
			err := target.cacheLocks[artifact.Name()].Unlock()
			if err != nil {
				log.Errorf("unlock %v %v: %v", target.FQN, artifact.Name(), err)
			}
		}
	}()

	ctx, span := e.Observability.SpanLocalCacheStore(ctx, target.Target)
	defer span.EndError(rerr)

	dir := e.cacheDir(target).Abs()

	err := os.RemoveAll(dir)
	if err != nil {
		return err
	}

	err = os.MkdirAll(dir, os.ModePerm)
	if err != nil {
		return err
	}

	for _, artifact := range allArtifacts {
		_, err := artifacts.GenArtifact(ctx, dir, artifact, artifacts.GenContext{
			OutRoot:     outRoot,
			LogFilePath: logFilePath,
		})
		if err != nil {
			return fmt.Errorf("genartifact %v: %w", artifact.Name(), err)
		}
	}

	err = fs.CreateParentDir(dir)
	if err != nil {
		return err
	}

	return e.linkLatestCache(target, dir)
}
