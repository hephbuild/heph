package engine

import (
	"context"
	"github.com/hephbuild/heph/buildfiles"
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
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/worker"
	"path/filepath"
	"sort"
	"strings"
	"sync"
)

type Engine struct {
	Cwd              string
	Root             *hroot.State
	Config           *graph.Config
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

	DisableNamedCacheWrite bool

	toolsLock locks.Locker

	RanGenPass bool
}

type TargetRunRequests []targetrun.Request

func (rrs TargetRunRequests) Has(t *graph.Target) bool {
	for _, rr := range rrs {
		if rr.Target.Addr == t.Addr {
			return true
		}
	}

	return false
}

func (rrs TargetRunRequests) Get(t *graph.Target) targetrun.Request {
	for _, rr := range rrs {
		if rr.Target.Addr == t.Addr {
			return rr
		}
	}

	return targetrun.Request{Target: t}
}

func (rrs TargetRunRequests) Targets() *graph.Targets {
	ts := graph.NewTargets(len(rrs))

	for _, rr := range rrs {
		ts.Add(rr.Target)
	}

	return ts
}

func (rrs TargetRunRequests) Count(f func(rr targetrun.Request) bool) int {
	c := 0
	for _, rr := range rrs {
		if f(rr) {
			c++
		}
	}

	return c
}

func New(e Engine) *Engine {
	e.toolsLock = locks.NewFlock("Tools", e.Root.Home.Join("tmp", "tools.lock").Abs())
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

func (e *Engine) ScheduleTargetsWithDeps(ctx context.Context, targets []*graph.Target, skip []specs.Specer) (*WaitGroupMap, error) {
	rrs := ads.Map(targets, func(t *graph.Target) targetrun.Request {
		return targetrun.Request{Target: t}
	})

	return e.ScheduleTargetRRsWithDeps(ctx, rrs, skip)
}

type keyFgWaitGroup struct{}

func ContextWithForegroundWaitGroup(ctx context.Context) (context.Context, *worker.WaitGroup) {
	deps := &worker.WaitGroup{}
	ctx = context.WithValue(ctx, keyFgWaitGroup{}, deps)

	return ctx, deps
}

func ForegroundWaitGroup(ctx context.Context) *worker.WaitGroup {
	if deps, ok := ctx.Value(keyFgWaitGroup{}).(*worker.WaitGroup); ok {
		return deps
	}

	return nil
}

func (e *Engine) CleanTarget(target *graph.Target, async bool) error {
	err := e.LocalCache.CleanTarget(target, async)
	if err != nil {
		return err
	}

	return nil
}

func (e *Engine) GetFileDeps(targets ...*graph.Target) []xfs.Path {
	return e.getFileDeps(targets, func(target *graph.Target) graph.TargetDeps {
		return target.Deps.All()
	})
}

func (e *Engine) GetFileHashDeps(targets ...*graph.Target) []xfs.Path {
	return e.getFileDeps(targets, func(target *graph.Target) graph.TargetDeps {
		return target.HashDeps
	})
}

func (e *Engine) getFileDeps(targets []*graph.Target, f func(*graph.Target) graph.TargetDeps) []xfs.Path {
	filesm := map[string]xfs.Path{}
	for _, target := range targets {
		for _, file := range f(target).Files {
			filesm[file.Abs()] = file
		}
	}

	files := make([]xfs.Path, 0, len(filesm))
	for _, file := range filesm {
		files = append(files, file)
	}

	sort.SliceStable(files, func(i, j int) bool {
		return files[i].RelRoot() < files[j].RelRoot()
	})

	return files
}

func (e *Engine) GetWatcherList(files []xfs.Path) []string {
	pm := map[string]struct{}{}

	for _, file := range files {
		filep := file.Abs()
		for {
			filep = filepath.Dir(filep)
			if filep == "." || filep == "/" {
				break
			}

			if !strings.HasPrefix(filep, e.Root.Root.Abs()) {
				break
			}

			pm[filep] = struct{}{}
		}
	}

	paths := make([]string, 0)
	for p := range pm {
		paths = append(paths, p)
	}
	sort.Strings(paths)

	return paths
}
