package engine

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/buildfiles"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/hroot"
	"github.com/hephbuild/heph/lcache"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/observability"
	"github.com/hephbuild/heph/packages"
	"github.com/hephbuild/heph/platform"
	"github.com/hephbuild/heph/rcache"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/finalizers"
	"github.com/hephbuild/heph/utils/instance"
	"github.com/hephbuild/heph/utils/locks"
	"github.com/hephbuild/heph/utils/sets"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/worker"
	"io/fs"
	"path/filepath"
	"sort"
	"strings"
	"sync"
)

type Engine struct {
	Cwd               string
	Root              *hroot.State
	Config            *graph.Config
	Observability     *observability.Observability
	GetFlowID         func() string
	PlatformProviders []platform.PlatformProvider
	LocalCache        *lcache.LocalCacheState
	RemoteCache       *rcache.RemoteCache
	RemoteCacheHints  *rcache.HintStore
	Packages          *packages.Registry
	BuildFilesState   *buildfiles.State
	Graph             *graph.State
	Pool              *worker.Pool
	Finalizers        *finalizers.Finalizers

	DisableNamedCacheWrite bool

	toolsLock             locks.Locker
	autocompleteCacheLock locks.Locker

	orderedCachesLock locks.Locker
	orderedCaches     []graph.CacheConfig

	Targets *TargetMetas

	RanGenPass bool
}

type TargetRunRequest struct {
	Target *graph.Target
	Args   []string
	Mode   string // run or watch
	TargetRunRequestOpts
}

type TargetRunRequestOpts struct {
	NoCache bool
	Shell   bool
	// Force preserving cache for uncached targets when --print-out is enabled
	PreserveCache bool
	NoPTY         bool
}

type TargetRunRequests []TargetRunRequest

func (rrs TargetRunRequests) Has(t *Target) bool {
	for _, rr := range rrs {
		if rr.Target.FQN == t.FQN {
			return true
		}
	}

	return false
}

func (rrs TargetRunRequests) Get(t *graph.Target) TargetRunRequest {
	for _, rr := range rrs {
		if rr.Target.FQN == t.FQN {
			return rr
		}
	}

	return TargetRunRequest{Target: t}
}

func (rrs TargetRunRequests) Targets() *graph.Targets {
	ts := graph.NewTargets(len(rrs))

	for _, rr := range rrs {
		ts.Add(rr.Target)
	}

	return ts
}

func (rrs TargetRunRequests) Count(f func(rr TargetRunRequest) bool) int {
	c := 0
	for _, rr := range rrs {
		if f(rr) {
			c++
		}
	}

	return c
}

func New(e Engine) *Engine {
	e.Targets = NewTargetMetas(func(fqn string) *Target {
		gtarget := e.Graph.Targets().Find(fqn)
		if gtarget == nil {
			return nil
		}

		t := &Target{
			Target:           gtarget,
			WorkdirRoot:      xfs.Path{},
			SandboxRoot:      xfs.Path{},
			OutExpansionRoot: nil, // Set during execution
			runLock:          nil, // Set after
			postRunWarmLock:  e.lockFactory(gtarget, "postrunwarm"),
		}
		if t.ConcurrentExecution {
			t.runLock = locks.NewMutex(t.FQN)
		} else {
			t.runLock = e.lockFactory(t, "run")
		}

		t.SandboxRoot = e.sandboxRoot(t).Join("_dir")

		t.WorkdirRoot = e.Root.Root
		if t.Sandbox {
			t.WorkdirRoot = t.SandboxRoot
		}

		return t
	})
	e.toolsLock = locks.NewFlock("Tools", e.Root.Home.Join("tmp", "tools.lock").Abs())
	e.autocompleteCacheLock = locks.NewFlock("Autocomplete cache", e.Root.Home.Join("tmp", "ac_cache.lock").Abs())
	e.orderedCachesLock = locks.NewFlock("Order cache", e.Root.Home.Join("tmp", "order_cache.lock").Abs())
	return &e
}

func (e *Engine) lockFactory(t specs.Specer, resource string) locks.Locker {
	ts := t.Spec()
	p := e.lockPath(t, resource)

	return locks.NewFlock(ts.FQN+" ("+resource+")", p)
}

type ErrorWithLogFile struct {
	LogFile string
	Err     error
}

func (t ErrorWithLogFile) Error() string {
	return t.Err.Error()
}

func (t ErrorWithLogFile) Unwrap() error {
	return t.Err
}

func (t ErrorWithLogFile) Is(target error) bool {
	_, ok := target.(TargetFailedError)

	return ok
}

type TargetFailedError struct {
	Target *graph.Target
	Err    error
}

func (t TargetFailedError) Error() string {
	return t.Err.Error()
}

func (t TargetFailedError) Unwrap() error {
	return t.Err
}

func (t TargetFailedError) Is(target error) bool {
	_, ok := target.(TargetFailedError)

	return ok
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
	rrs := make([]TargetRunRequest, 0, len(targets))
	for _, target := range targets {
		rrs = append(rrs, TargetRunRequest{Target: target})
	}

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

func (e *Engine) collectNamedOut(target *Target, namedPaths *tgt.OutNamedPaths, root string) (*ActualOutNamedPaths, error) {
	tp := &ActualOutNamedPaths{}

	for name, paths := range namedPaths.Named() {
		tp.ProvisionName(name)

		files, err := e.collectOut(target, paths, root)
		if err != nil {
			return nil, err
		}

		for _, file := range files {
			tp.Add(name, file)
		}
	}

	return tp, nil
}

func (e *Engine) collectNamedOutFromTar(ctx context.Context, target *Target, outputs []string) (*ActualOutNamedPaths, error) {
	tp := &ActualOutNamedPaths{}

	for name := range target.Out.Named() {
		if !ads.Contains(outputs, name) {
			continue
		}

		tp.ProvisionName(name)

		artifact := target.Artifacts.OutTar(name)

		files, err := e.collectOutFromArtifact(ctx, target, artifact)
		if err != nil {
			return nil, err
		}

		for _, file := range files {
			tp.Add(name, file.WithRoot(target.OutExpansionRoot.Abs()))
		}
	}

	tp.Sort()

	return tp, nil
}

func (e *Engine) collectOutFromArtifact(ctx context.Context, target *Target, artifact artifacts.Artifact) (xfs.Paths, error) {
	paths, err := e.LocalCache.PathsFromArtifact(ctx, target, artifact)
	if err != nil {
		return nil, err
	}

	return paths.WithRoot(target.OutExpansionRoot.Abs()), nil
}

func (e *Engine) collectOut(target *Target, files xfs.RelPaths, root string) (xfs.Paths, error) {
	outSet := sets.NewSet(func(p xfs.Path) string {
		return p.RelRoot()
	}, len(files))

	for _, file := range files {
		pattern := file.RelRoot()

		if !xfs.IsGlob(pattern) && !xfs.PathExists(filepath.Join(root, pattern)) {
			return nil, fmt.Errorf("%v did not output %v", target.FQN, pattern)
		}

		err := xfs.StarWalk(root, pattern, nil, func(path string, d fs.DirEntry, err error) error {
			outSet.Add(xfs.NewPath(root, path))

			return nil
		})
		if err != nil {
			return nil, fmt.Errorf("collect output %v: %w", file.RelRoot(), err)
		}
	}

	out := xfs.Paths(outSet.Slice())
	out.Sort()

	return out, nil
}

func (e *Engine) populateActualFiles(ctx context.Context, target *Target, outRoot string) (rerr error) {
	ctx, span := e.Observability.SpanCollectOutput(ctx, target.Target.Target)
	defer span.EndError(rerr)

	target.actualOutFiles = &ActualOutNamedPaths{}
	target.actualSupportFiles = make(xfs.Paths, 0)

	var err error

	target.actualOutFiles, err = e.collectNamedOut(target, target.Out, outRoot)
	if err != nil {
		return fmt.Errorf("out: %w", err)
	}

	if target.HasSupportFiles {
		target.actualSupportFiles, err = e.collectOut(target, target.OutWithSupport.Name(specs.SupportFilesOutput), outRoot)
		if err != nil {
			return fmt.Errorf("support: %w", err)
		}
	}

	return nil
}

func (e *Engine) populateActualFilesFromTar(ctx context.Context, target *Target, outputs []string) error {
	log.Tracef("populateActualFilesFromTar %v", target.FQN)

	target.actualOutFiles = &ActualOutNamedPaths{}
	target.actualSupportFiles = make(xfs.Paths, 0)

	var err error

	target.actualOutFiles, err = e.collectNamedOutFromTar(ctx, target, outputs)
	if err != nil {
		return fmt.Errorf("out: %w", err)
	}

	if target.HasSupportFiles {
		art := target.Artifacts.OutTar(specs.SupportFilesOutput)

		target.actualSupportFiles, err = e.collectOutFromArtifact(ctx, target, art)
		if err != nil {
			return fmt.Errorf("support: %w", err)
		}
	}

	return nil
}

func (e *Engine) sandboxRoot(specer specs.Specer) xfs.Path {
	target := specer.Spec()

	folder := "__target_" + target.Name
	if target.ConcurrentExecution {
		folder = "__target_tmp_" + instance.UID + "_" + target.Name
	}

	p := e.Root.Home.Join("sandbox", target.Package.Path, folder)

	if target.ConcurrentExecution {
		e.Finalizers.RegisterRemove(p.Abs())
	}

	return p
}

func (e *Engine) CleanTarget(target *Target, async bool) error {
	sandboxDir := e.sandboxRoot(target)
	err := xfs.DeleteDir(sandboxDir.Abs(), async)
	if err != nil {
		return err
	}

	err = e.LocalCache.CleanTarget(target, async)
	if err != nil {
		return err
	}

	return nil
}

func (e *Engine) CleanTargetLock(target *Target) error {
	err := target.runLock.Clean()
	if err != nil {
		return err
	}

	err = e.LocalCache.CleanTargetLock(target)
	if err != nil {
		return err
	}

	return nil
}

func (e *Engine) GetFileDeps(targets ...*Target) []xfs.Path {
	return e.getFileDeps(targets, func(target *Target) tgt.TargetDeps {
		return target.Deps.All()
	})
}

func (e *Engine) GetFileHashDeps(targets ...*Target) []xfs.Path {
	return e.getFileDeps(targets, func(target *Target) tgt.TargetDeps {
		return target.HashDeps
	})
}

func (e *Engine) getFileDeps(targets []*Target, f func(*Target) tgt.TargetDeps) []xfs.Path {
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
