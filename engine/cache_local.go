package engine

import (
	"context"
	"fmt"
	"heph/engine/artifacts"
	log "heph/hlog"
	"heph/utils/fs"
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

	span := e.SpanCacheStore(ctx, target)
	defer func() {
		span.EndError(rerr)
	}()

	dir := e.cacheDir(target).Abs()

	err := os.RemoveAll(dir)
	if err != nil {
		return err
	}

	err = os.MkdirAll(dir, os.ModePerm)
	if err != nil {
		return err
	}

	// TODO: look into schedule on pool
	for _, artifact := range allArtifacts {
		_, err := artifacts.GenArtifact(ctx, dir, artifact, artifacts.GenContext{
			OutRoot:     outRoot,
			LogFilePath: logFilePath,
		})
		if err != nil {
			return err
		}
	}

	err = fs.CreateParentDir(dir)
	if rerr != nil {
		return rerr
	}

	return e.linkLatestCache(target, dir)
}
