package scheduler

import (
	"context"
	"github.com/hephbuild/heph/buildfiles"
	"github.com/hephbuild/heph/config"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/hroot"
	"github.com/hephbuild/heph/lcache"
	"github.com/hephbuild/heph/observability"
	"github.com/hephbuild/heph/packages"
	"github.com/hephbuild/heph/rcache"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/targetrun"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/finalizers"
	"github.com/hephbuild/heph/utils/locks"
	"github.com/hephbuild/heph/worker"
	"sync"
)

type Scheduler struct {
	Cwd              string
	Root             *hroot.State
	Config           *config.Config
	Observability    *observability.Observability
	GetFlowID        func() string
	LocalCache       *lcache.LocalCacheState
	RemoteCache      *rcache.RemoteCache
	RemoteCacheHints *rcache.HintStore
	Packages         *packages.Registry
	BuildFilesState  *buildfiles.State
	Graph            *graph.State
	Pool             *worker.Pool
	Finalizers       *finalizers.Finalizers
	Runner           *targetrun.Runner

	toolsLock locks.Locker
}

func New(e Scheduler) *Scheduler {
	e.toolsLock = locks.NewFlock("Tools", e.Root.Tmp.Join("tools.lock").Abs())
	return &e
}

type WaitGroupMap struct {
	mu sync.Mutex
	m  map[string]*worker.WaitGroup
}

func (wgm *WaitGroupMap) All() *worker.WaitGroup {
	wgm.mu.Lock()
	defer wgm.mu.Unlock()

	wg := &worker.WaitGroup{}

	for _, e := range wgm.m {
		wg.AddChild(e)
	}

	return wg
}

func (wgm *WaitGroupMap) Get(s string) *worker.WaitGroup {
	wgm.mu.Lock()
	defer wgm.mu.Unlock()

	if wg, ok := wgm.m[s]; ok {
		return wg
	}

	if wgm.m == nil {
		wgm.m = map[string]*worker.WaitGroup{}
	}

	wg := &worker.WaitGroup{}
	wgm.m[s] = wg

	return wg
}

func (e *Scheduler) ScheduleTargetsWithDeps(ctx context.Context, targets []*graph.Target, pullCache bool, skip []specs.Specer) (*WaitGroupMap, error) {
	rrs := ads.Map(targets, func(t *graph.Target) targetrun.Request {
		return targetrun.Request{Target: t, RequestOpts: targetrun.RequestOpts{PullCache: pullCache}}
	})

	return e.ScheduleTargetRRsWithDeps(ctx, rrs, skip)
}

func (e *Scheduler) CleanTarget(target *graph.Target, async bool) error {
	err := e.LocalCache.CleanTarget(target, async)
	if err != nil {
		return err
	}

	return nil
}
