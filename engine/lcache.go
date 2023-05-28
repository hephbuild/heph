package engine

import (
	"context"
	"errors"
	"fmt"
	vfsos "github.com/c2fo/vfs/v6/backend/os"
	"github.com/hephbuild/heph/engine/graph"
	"github.com/hephbuild/heph/engine/hroot"
	"github.com/hephbuild/heph/engine/observability"
	"github.com/hephbuild/heph/engine/status"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/utils/flock"
	"github.com/hephbuild/heph/utils/fs"
	"github.com/hephbuild/heph/utils/maps"
	"github.com/hephbuild/heph/vfssimple"
	"os"
	"strings"
	"time"
)

type LocalCacheState struct {
	Location      *vfsos.Location
	Path          fs.Path
	Targets       *TargetMetas
	Root          *hroot.State
	Graph         *graph.State
	Observability *observability.Observability

	cacheHashInputTargetMutex  maps.KMutex
	cacheHashInput             *maps.Map[string, string]
	cacheHashOutputTargetMutex maps.KMutex
	cacheHashOutput            *maps.Map[string, string] // TODO: LRU
	gcLock                     flock.Locker
	cacheHashInputPathsModtime *maps.Map[string, map[string]time.Time]
}

func NewState(root *hroot.State, g *graph.State, obs *observability.Observability) (*LocalCacheState, error) {
	cachePath := root.Home.Join("cache")
	loc, err := vfssimple.NewLocation("file://" + cachePath.Abs() + "/")
	if err != nil {
		return nil, fmt.Errorf("lcache location: %w", err)
	}

	s := &LocalCacheState{
		Location:                   loc.(*vfsos.Location),
		Path:                       cachePath,
		Targets:                    nil, //  Will be set manually after Engine init
		Root:                       root,
		Graph:                      g,
		Observability:              obs,
		cacheHashInputTargetMutex:  maps.KMutex{},
		cacheHashInput:             &maps.Map[string, string]{},
		cacheHashOutputTargetMutex: maps.KMutex{},
		cacheHashOutput:            &maps.Map[string, string]{},
		cacheHashInputPathsModtime: &maps.Map[string, map[string]time.Time]{},
		gcLock:                     flock.NewFlock("Global GC", root.Home.Join("tmp", "gc.lock").Abs()),
	}

	return s, nil
}

func (e *LocalCacheState) storeCache(ctx context.Context, target *Target, outRoot string, logFilePath string, compress bool) (rerr error) {
	if target.ConcurrentExecution {
		log.Debugf("%v concurrent execution, skipping storeCache", target.FQN)
		return nil
	}

	if target.Cache.Enabled {
		status.Emit(ctx, TargetStatus(target, "Caching..."))
	} else if len(target.Artifacts.Out) > 0 {
		status.Emit(ctx, TargetStatus(target, "Storing output..."))
	}

	ctx, span := e.Observability.SpanLocalCacheStore(ctx, target.Target.Target)
	defer span.EndError(rerr)

	allArtifacts := e.orderedArtifactProducers(target, outRoot, logFilePath)

	dir := e.cacheDir(target).Abs()

	err := os.RemoveAll(dir)
	if err != nil {
		return err
	}

	err = os.MkdirAll(dir, os.ModePerm)
	if err != nil {
		return err
	}

	err = e.GenArtifacts(ctx, dir, target, allArtifacts, compress)
	if err != nil {
		return err
	}

	err = fs.CreateParentDir(dir)
	if err != nil {
		return err
	}

	return e.linkLatestCache(target, dir)
}

func (e *LocalCacheState) linkLatestCache(target *Target, from string) error {
	latestDir := e.cacheDirForHash(target, "latest")

	err := os.RemoveAll(latestDir.Abs())
	if err != nil {
		return err
	}

	err = os.Symlink(from, latestDir.Abs())
	if err != nil && !errors.Is(err, os.ErrExist) {
		return err
	}

	return nil
}

func (e *LocalCacheState) ResetCacheHashInput(spec targetspec.Specer) {
	target := spec.Spec()

	e.cacheHashInput.DeleteP(func(k string) bool {
		return strings.HasPrefix(k, target.FQN)
	})

	e.cacheHashInputPathsModtime.DeleteP(func(k string) bool {
		return strings.HasPrefix(k, target.FQN)
	})
}
