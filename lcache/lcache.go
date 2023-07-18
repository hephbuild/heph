package lcache

import (
	"context"
	"errors"
	"fmt"
	vfsos "github.com/c2fo/vfs/v6/backend/os"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/hroot"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/observability"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/locks"
	"github.com/hephbuild/heph/utils/maps"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/vfssimple"
	"os"
	"time"
)

type targetCacheKey struct {
	fqn  string
	hash string
}

func (k targetCacheKey) String() string {
	return k.fqn + "_" + k.hash
}

type targetOutCacheKey struct {
	fqn    string
	output string
	hash   string
}

func (k targetOutCacheKey) String() string {
	return k.fqn + "|" + k.output + "_" + k.hash
}

type LocalCacheState struct {
	Location      *vfsos.Location
	Path          xfs.Path
	Targets       *graph.Targets
	TargetMetas   *TargetMetas
	Root          *hroot.State
	Graph         *graph.State
	Observability *observability.Observability

	cacheHashInputTargetMutex  maps.KMutex
	cacheHashInput             *maps.Map[targetCacheKey, string]
	cacheHashOutputTargetMutex maps.KMutex
	cacheHashOutput            *maps.Map[targetOutCacheKey, string] // TODO: LRU
	cacheHashInputPathsModtime *maps.Map[targetCacheKey, map[string]time.Time]
}

func NewState(root *hroot.State, g *graph.State, obs *observability.Observability) (*LocalCacheState, error) {
	cachePath := root.Home.Join("cache")
	loc, err := vfssimple.NewLocation("file://" + cachePath.Abs() + "/")
	if err != nil {
		return nil, fmt.Errorf("lcache location: %w", err)
	}

	s := &LocalCacheState{
		Location:      loc.(*vfsos.Location),
		Path:          cachePath,
		Targets:       g.Targets(),
		Root:          root,
		Graph:         g,
		Observability: obs,
		TargetMetas: NewTargetMetas(func(fqn string) *Target {
			gtarget := g.Targets().Find(fqn)

			t := &Target{
				Target:     gtarget,
				cacheLocks: map[string]locks.Locker{},
			}

			t.cacheLocks = make(map[string]locks.Locker, len(t.Artifacts.All()))
			for _, artifact := range t.Artifacts.All() {
				ts := t.Spec()
				resource := artifact.Name()

				p := lockPath(root, t, "cache_"+resource)

				l := locks.NewFlock(ts.FQN+" ("+resource+")", p)

				t.cacheLocks[artifact.Name()] = l
			}

			return t
		}),
		cacheHashInputTargetMutex:  maps.KMutex{},
		cacheHashInput:             &maps.Map[targetCacheKey, string]{},
		cacheHashOutputTargetMutex: maps.KMutex{},
		cacheHashOutput:            &maps.Map[targetOutCacheKey, string]{},
		cacheHashInputPathsModtime: &maps.Map[targetCacheKey, map[string]time.Time]{},
	}

	return s, nil
}

func (e *LocalCacheState) StoreCache(ctx context.Context, ttarget graph.Targeter, allArtifacts []ArtifactWithProducer, compress bool) (rerr error) {
	target := ttarget.GraphTarget()

	if target.ConcurrentExecution {
		log.Debugf("%v concurrent execution, skipping storeCache", target.FQN)
		return nil
	}

	if target.Cache.Enabled {
		status.Emit(ctx, tgt.TargetStatus(target, "Caching..."))
	} else if len(target.Artifacts.Out) > 0 {
		status.Emit(ctx, tgt.TargetStatus(target, "Storing output..."))
	}

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

	err = e.GenArtifacts(ctx, dir, target, allArtifacts, compress)
	if err != nil {
		return err
	}

	err = xfs.CreateParentDir(dir)
	if err != nil {
		return err
	}

	return e.linkLatestCache(target, dir)
}

func (e *LocalCacheState) linkLatestCache(target targetspec.Specer, from string) error {
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

	e.cacheHashInput.DeleteP(func(k targetCacheKey) bool {
		return k.fqn == target.FQN
	})

	e.cacheHashInputPathsModtime.DeleteP(func(k targetCacheKey) bool {
		return k.fqn == target.FQN
	})
}
