package engine

import (
	"context"
	"errors"
	"fmt"
	"github.com/c2fo/vfs/v6"
	vfsos "github.com/c2fo/vfs/v6/backend/os"
	"github.com/charmbracelet/lipgloss"
	"github.com/heimdalr/dag"
	"go.opentelemetry.io/otel/trace"
	"go.starlark.net/starlark"
	"heph/config"
	"heph/engine/htrace"
	log "heph/hlog"
	"heph/packages"
	"heph/platform"
	"heph/sandbox"
	"heph/targetspec"
	"heph/tgt"
	"heph/utils"
	"heph/utils/flock"
	fs2 "heph/utils/fs"
	"heph/utils/hash"
	"heph/utils/maps"
	"heph/utils/sets"
	"heph/utils/tar"
	"heph/vfssimple"
	"heph/worker"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"syscall"
)

type Engine struct {
	Cwd               string
	Root              fs2.Path
	HomeDir           fs2.Path
	Config            Config
	LocalCache        *vfsos.Location
	Stats             *htrace.Stats
	Tracer            trace.Tracer
	RootSpan          trace.Span
	PlatformProviders []PlatformProvider

	DisableNamedCacheWrite bool

	SourceFiles   packages.SourceFiles
	packagesMutex sync.Mutex
	Packages      map[string]*packages.Package

	TargetsLock maps.KMutex
	Targets     *Targets
	Labels      *sets.StringSet

	Params map[string]string

	dag *DAG

	autocompleteHash string

	cacheHashInputTargetMutex  maps.KMutex
	cacheHashInput             *maps.Map[string, string]
	cacheHashOutputTargetMutex maps.KMutex
	cacheHashOutput            *maps.Map[string, string] // TODO: LRU
	RanGenPass                 bool
	RanInit                    bool
	codegenPaths               map[string]*Target
	tools                      *Targets
	fetchRootCache             map[string]fs2.Path
	cacheRunBuildFilem         sync.RWMutex
	cacheRunBuildFile          map[string]starlark.StringDict
	Pool                       *worker.Pool

	exitHandlersm       sync.Mutex
	exitHandlers        []func()
	exitHandlersRunning bool

	gcLock    flock.Locker
	toolsLock flock.Locker
}

type PlatformProvider struct {
	config.Platform
	platform.Provider
}

type Config struct {
	config.Config
	Profiles []string
	Cache    []CacheConfig
}

type CacheConfig struct {
	Name string
	config.Cache
	Location vfs.Location `yaml:"-"`
}

type RunStatus struct {
	Description string
}

type TargetRunRequest struct {
	Target  *Target
	Args    []string
	NoCache bool
	Shell   bool
	Mode    string // run or watch
	// Force preserving cache for uncached targets when --print-out is enabled
	PreserveCache bool
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

func (rrs TargetRunRequests) Get(t *Target) TargetRunRequest {
	for _, rr := range rrs {
		if rr.Target.FQN == t.FQN {
			return rr
		}
	}

	return TargetRunRequest{Target: t}
}

func (rrs TargetRunRequests) Targets() []*Target {
	ts := make([]*Target, 0, len(rrs))

	for _, rr := range rrs {
		ts = append(ts, rr.Target)
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

func New(rootPath string) *Engine {
	root := fs2.NewPath(rootPath, "")

	homeDir := root.Join(".heph")

	log.Tracef("home dir %v", homeDir.Abs())

	err := os.MkdirAll(homeDir.Abs(), os.ModePerm)
	if err != nil {
		log.Fatal(fmt.Errorf("create homedir %v: %w", homeDir, err))
	}

	cacheDir := homeDir.Join("cache")

	loc, err := vfssimple.NewLocation("file://" + cacheDir.Abs() + "/")
	if err != nil {
		log.Fatal(fmt.Errorf("cache location: %w", err))
	}

	return &Engine{
		Root:              root,
		HomeDir:           homeDir,
		LocalCache:        loc.(*vfsos.Location),
		Targets:           NewTargets(0),
		Stats:             &htrace.Stats{},
		Tracer:            trace.NewNoopTracerProvider().Tracer(""),
		Packages:          map[string]*packages.Package{},
		cacheHashInput:    &maps.Map[string, string]{},
		cacheHashOutput:   &maps.Map[string, string]{},
		codegenPaths:      map[string]*Target{},
		tools:             NewTargets(0),
		fetchRootCache:    map[string]fs2.Path{},
		cacheRunBuildFile: map[string]starlark.StringDict{},
		Labels:            sets.NewStringSet(0),
		gcLock:            flock.NewFlock("Global GC", homeDir.Join("tmp", "gc.lock").Abs()),
		toolsLock:         flock.NewFlock("Tools", homeDir.Join("tmp", "tools.lock").Abs()),
		dag:               &DAG{dag.NewDAG()},
	}
}

func (e *Engine) DAG() *DAG {
	return e.dag
}

func (e *Engine) CodegenPaths() map[string]*Target {
	return e.codegenPaths
}

func hashCacheId(target *Target) string {
	idh := hash.NewHash()
	for _, fqn := range target.linkingDeps.FQNs() {
		idh.String(fqn)
	}

	return target.FQN + idh.Sum()
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
	Target *Target
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

func (e *Engine) ScheduleTargetsWithDeps(ctx context.Context, targets []*Target, skip *Target) (*WaitGroupMap, error) {
	rrs := make([]TargetRunRequest, 0, len(targets))
	for _, target := range targets {
		rrs = append(rrs, TargetRunRequest{Target: target})
	}

	return e.ScheduleTargetRRsWithDeps(ctx, rrs, skip)
}

var schedulerV2Once sync.Once

func (e *Engine) ScheduleTargetRRsWithDeps(ctx context.Context, rrs TargetRunRequests, skip *Target) (*WaitGroupMap, error) {
	if e.Config.TargetScheduler == "v2" {
		schedulerV2Once.Do(func() {
			log.Info("Scheduler v2")
		})
		return e.ScheduleV2TargetRRsWithDeps(ctx, rrs, skip)
	}

	return e.ScheduleV1TargetRRsWithDeps(ctx, rrs, skip)
}

func (e *Engine) ScheduleTargetCacheWarm(ctx context.Context, target *Target, outputs []string, wdeps *worker.WaitGroup) (*worker.Job, error) {
	postDeps := &worker.WaitGroup{}
	for _, output := range append(outputs, inputHashName) {
		output := output

		j := e.Pool.Schedule(ctx, &worker.Job{
			Name: "warm " + target.FQN + "|" + output,
			Deps: wdeps,
			Do: func(w *worker.Worker, ctx context.Context) error {
				e := NewTargetRunEngine(e, w.Status)

				w.Status(TargetOutputStatus(target, output, "Priming cache..."))

				cached, err := e.WarmTargetCache(ctx, target, output)
				if err != nil {
					return err
				}

				if cached {
					return nil
				}

				return nil
			},
		})
		postDeps.Add(j)
	}

	return e.ScheduleTargetPostRunOrWarm(ctx, target, postDeps, outputs), nil
}

func TargetStatus(t *Target, status string) worker.Status {
	return targetStatus{t.FQN, "", status}
}

type targetStatus struct {
	fqn, output, status string
}

var targetStyle = struct {
	target, output lipgloss.Style
}{
	lipgloss.NewStyle().Foreground(lipgloss.Color("#FFCA33")),
	lipgloss.NewStyle().Foreground(lipgloss.Color("#FFCA33")).Bold(true),
}

func (t targetStatus) String(term bool) string {
	var target, output lipgloss.Style
	if term {
		target, output = targetStyle.target, targetStyle.output
	}

	outputStr := ""
	if t.output != "" {
		outputStr = output.Render("|" + t.output)
	}

	return target.Render(t.fqn) + outputStr + " " + t.status
}

func TargetOutputStatus(t *Target, output string, status string) worker.Status {
	if output == "" {
		output = "-"
	}
	return targetStatus{t.FQN, output, status}
}

func (e *Engine) ScheduleTargetPostRunOrWarm(ctx context.Context, target *Target, deps *worker.WaitGroup, outputs []string) *worker.Job {
	return e.Pool.Schedule(ctx, &worker.Job{
		Name: "postrunwarm " + target.FQN,
		Deps: deps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			e := NewTargetRunEngine(e, w.Status)

			return e.postRunOrWarm(ctx, target, outputs)
		},
	})
}

func (e *Engine) ScheduleTarget(ctx context.Context, rr TargetRunRequest, deps *worker.WaitGroup) (*worker.Job, error) {
	j := e.Pool.Schedule(ctx, &worker.Job{
		Name: rr.Target.FQN,
		Deps: deps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			e := NewTargetRunEngine(e, w.Status)

			err := e.Run(ctx, rr, sandbox.IOConfig{})
			if err != nil {
				return TargetFailedError{
					Target: rr.Target,
					Err:    err,
				}
			}

			return nil
		},
	})

	return j, nil
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

func (e *Engine) collectNamedOutFromTar(target *Target, namedPaths *tgt.OutNamedPaths) (*ActualOutNamedPaths, error) {
	tp := &ActualOutNamedPaths{}

	for name := range namedPaths.Named() {
		p := e.targetOutputTarFile(target, name)
		if !fs2.PathExists(p) {
			continue
		}

		tp.ProvisionName(name)

		files, err := e.collectOutFromTar(target, p)
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

func (e *Engine) collectOutFromTar(target *Target, tarPath string) (fs2.Paths, error) {
	files, err := tar.UntarList(context.Background(), tarPath)
	if err != nil {
		return nil, err
	}

	ps := make(fs2.Paths, len(files))
	for i, file := range files {
		ps[i] = fs2.NewRelPath(file).WithRoot(target.OutExpansionRoot.Abs())
	}

	ps.Sort()

	return ps, nil
}

func (e *Engine) collectOut(target *Target, files fs2.RelPaths, root string) (fs2.Paths, error) {
	outSet := sets.NewSet(func(p fs2.Path) string {
		return p.RelRoot()
	}, len(files))

	for _, file := range files {
		pattern := file.RelRoot()

		if !utils.IsGlob(pattern) && !fs2.PathExists(filepath.Join(root, pattern)) {
			return nil, fmt.Errorf("%v did not output %v", target.FQN, file.RelRoot())
		}

		err := utils.StarWalk(root, pattern, nil, func(path string, d fs.DirEntry, err error) error {
			outSet.Add(fs2.NewPath(root, path))

			return nil
		})
		if err != nil {
			return nil, fmt.Errorf("collect output %v: %w", file.RelRoot(), err)
		}
	}

	out := fs2.Paths(outSet.Slice())
	out.Sort()

	return out, nil
}

func (e *TargetRunEngine) populateActualFiles(ctx context.Context, target *Target, outRoot string) (rerr error) {
	span := e.SpanCollectOutput(ctx, target)
	defer func() {
		span.EndError(rerr)
	}()

	target.actualOutFiles = &ActualOutNamedPaths{}
	target.actualSupportFiles = make(fs2.Paths, 0)

	var err error

	target.actualOutFiles, err = e.collectNamedOut(target, target.Out, outRoot)
	if err != nil {
		return fmt.Errorf("out: %w", err)
	}

	if target.HasSupportFiles {
		target.actualSupportFiles, err = e.collectOut(target, target.OutWithSupport.Name(targetspec.SupportFilesOutput), outRoot)
		if err != nil {
			return fmt.Errorf("support: %w", err)
		}
	}

	return nil
}

func (e *Engine) populateActualFilesFromTar(target *Target) error {
	log.Tracef("populateActualFilesFromTar %v", target.FQN)

	target.actualOutFiles = &ActualOutNamedPaths{}
	target.actualSupportFiles = make(fs2.Paths, 0)

	var err error

	target.actualOutFiles, err = e.collectNamedOutFromTar(target, target.Out)
	if err != nil {
		return fmt.Errorf("out: %w", err)
	}

	if target.HasSupportFiles {
		target.actualSupportFiles, err = e.collectOutFromTar(target, e.targetOutputTarFile(target, targetspec.SupportFilesOutput))
		if err != nil {
			return fmt.Errorf("support: %w", err)
		}
	}

	return nil
}

func (e *Engine) sandboxRoot(target *Target) fs2.Path {
	folder := "__target_" + target.Name
	if target.ConcurrentExecution {
		folder = "__target_tmp_" + SOMEID + "_" + target.Name
	}

	p := e.HomeDir.Join("sandbox", target.Package.FullName, folder)

	if target.ConcurrentExecution {
		e.RegisterRemove(p.Abs())
	}

	return p
}

func (e *Engine) Clean(async bool) error {
	return deleteDir(e.HomeDir.Abs(), async)
}

func (e *Engine) CleanTarget(target *Target, async bool) error {
	sandboxDir := e.sandboxRoot(target)
	err := deleteDir(sandboxDir.Abs(), async)
	if err != nil {
		return err
	}

	cacheDir := e.cacheDirForHash(target, "")
	err = deleteDir(cacheDir.Abs(), async)
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

	for _, l := range target.cacheLocks {
		err = l.Clean()
		if err != nil {
			return err
		}
	}

	return nil
}

func deleteDir(dir string, async bool) error {
	rm, err := exec.LookPath("rm")
	if err != nil {
		return err
	} else if !fs2.PathExists(dir) {
		return nil // not an error, just don't need to do anything.
	}

	log.Tracef("Deleting %v", dir)

	if async {
		newDir := fs2.RandPath(os.TempDir(), filepath.Base(dir), "")

		err = os.Rename(dir, newDir)
		if err != nil {
			// May be because os.TempDir() and the current dir aren't on the same device, try a sibling folder
			newDir = fs2.RandPath(filepath.Dir(dir), filepath.Base(dir), "")

			err1 := os.Rename(dir, newDir)
			if err1 != nil {
				log.Warnf("rename failed %v, deleting synchronously", err)
				return deleteDir(dir, false)
			}
		}

		// Note that we can't fork() directly and continue running Go code, but ForkExec() works okay.
		// Hence why we're using rm rather than fork() + os.RemoveAll.
		_, err = syscall.ForkExec(rm, []string{rm, "-rf", newDir}, nil)
		return err
	}

	out, err := exec.Command(rm, "-rf", dir).CombinedOutput()
	if err != nil {
		return fmt.Errorf("failed to remove directory: %s", string(out))
	}

	return nil
}

func (e *Engine) parseConfigs() error {
	log.Tracef("Profiles: %v", e.Config.Profiles)

	cfg := config.Config{}
	cfg.BuildFiles.Ignore = append(cfg.BuildFiles.Ignore, "**/.heph")
	cfg.CacheHistory = 3
	cfg.TargetScheduler = "v1"
	cfg.Platforms = map[string]config.Platform{
		"local": {
			Name:     "local",
			Provider: "local",
			Priority: 100,
		},
	}

	err := config.ParseAndApply(e.Root.Join(".hephconfig").Abs(), &cfg)
	if err != nil {
		return err
	}

	err = config.ParseAndApply(e.Root.Join(".hephconfig.local").Abs(), &cfg)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}

	for _, profile := range e.Config.Profiles {
		err := config.ParseAndApply(e.Root.Join(".hephconfig."+profile).Abs(), &cfg)
		if err != nil && !errors.Is(err, os.ErrNotExist) {
			return err
		}
	}

	e.Config.Config = cfg

	for name, cache := range cfg.Cache {
		loc, err := vfssimple.NewLocation(cache.URI)
		if err != nil {
			return fmt.Errorf("cache %v :%w", name, err)
		}

		e.Config.Cache = append(e.Config.Cache, CacheConfig{
			Name:     name,
			Cache:    cache,
			Location: loc,
		})
	}

	return nil
}

func (e *Engine) HasLabel(label string) bool {
	return e.Labels.Has(label)
}

func (e *Engine) GeneratedTargets() []*Target {
	targets := make([]*Target, 0)

	for _, target := range e.Targets.Slice() {
		if !target.Gen {
			continue
		}

		targets = append(targets, target)
	}

	return targets
}

func (e *Engine) registerLabels(labels []string) {
	for _, label := range labels {
		e.Labels.Add(label)
	}
}

func (e *Engine) GetFileDeps(targets ...*Target) []fs2.Path {
	return e.getFileDeps(targets, func(target *Target) tgt.TargetDeps {
		return target.Deps.All()
	})
}

func (e *Engine) GetFileHashDeps(targets ...*Target) []fs2.Path {
	return e.getFileDeps(targets, func(target *Target) tgt.TargetDeps {
		return target.HashDeps
	})
}

func (e *Engine) getFileDeps(targets []*Target, f func(*Target) tgt.TargetDeps) []fs2.Path {
	filesm := map[string]fs2.Path{}
	for _, target := range targets {
		for _, file := range f(target).Files {
			filesm[file.Abs()] = file
		}
	}

	files := make([]fs2.Path, 0, len(filesm))
	for _, file := range filesm {
		files = append(files, file)
	}

	sort.SliceStable(files, func(i, j int) bool {
		return files[i].RelRoot() < files[j].RelRoot()
	})

	return files
}

func (e *Engine) GetWatcherList(files []fs2.Path) []string {
	pm := map[string]struct{}{}

	for _, file := range files {
		filep := file.Abs()
		for {
			filep = filepath.Dir(filep)
			if filep == "." || filep == "/" {
				break
			}

			if !strings.HasPrefix(filep, e.Root.Abs()) {
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

func (e *Engine) GetFileDescendants(paths []string, targets []*Target) ([]*Target, error) {
	descendants := NewTargets(0)

	for _, path := range paths {
		for _, target := range targets {
			for _, file := range target.HashDeps.Files {
				if file.RelRoot() == path {
					descendants.Add(target)
					break
				}
			}
		}
	}

	descendants.Sort()

	return descendants.Slice(), nil
}

func (e *Engine) RegisterRemove(path string) {
	e.RegisterExitHandler(func() {
		err := os.RemoveAll(path)
		if err != nil {
			log.Error(err)
		}
	})
}

func (e *Engine) RegisterExitHandler(f func()) {
	if e.exitHandlersRunning {
		return
	}

	e.exitHandlersm.Lock()
	defer e.exitHandlersm.Unlock()

	e.exitHandlers = append(e.exitHandlers, f)
}

func (e *Engine) RunExitHandlers() {
	e.exitHandlersm.Lock()
	defer e.exitHandlersm.Unlock()

	e.exitHandlersRunning = true
	defer func() {
		e.exitHandlersRunning = false
	}()

	for i := len(e.exitHandlers) - 1; i >= 0; i-- {
		h := e.exitHandlers[i]
		h()
	}

	e.exitHandlers = nil
}

func (e *Engine) GetCodegenOrigin(path string) (*Target, bool) {
	if dep, ok := e.codegenPaths[path]; ok {
		return dep, ok
	}

	for cpath, dep := range e.codegenPaths {
		if ok, _ := utils.PathMatch(path, cpath); ok {
			return dep, true
		}
	}

	return nil, false
}

func (e *Engine) linkedTargets() *Targets {
	targets := NewTargets(e.Targets.Len())

	for _, target := range e.Targets.Slice() {
		if !target.linked {
			continue
		}

		targets.Add(target)
	}

	return targets
}

func (e *Engine) toolTargets() *Targets {
	targets := NewTargets(e.Targets.Len())

	for _, target := range e.Targets.Slice() {
		if !target.IsTool() {
			continue
		}

		targets.Add(target)
	}

	return targets
}
