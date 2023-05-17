package engine

import (
	"context"
	"errors"
	"fmt"
	"github.com/c2fo/vfs/v6"
	vfsos "github.com/c2fo/vfs/v6/backend/os"
	"github.com/charmbracelet/lipgloss"
	"github.com/heimdalr/dag"
	"github.com/hephbuild/heph/cloudclient"
	"github.com/hephbuild/heph/config"
	"github.com/hephbuild/heph/engine/artifacts"
	"github.com/hephbuild/heph/engine/buildfiles"
	"github.com/hephbuild/heph/engine/observability"
	obsummary "github.com/hephbuild/heph/engine/observability/summary"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/packages"
	"github.com/hephbuild/heph/platform"
	"github.com/hephbuild/heph/rcache"
	"github.com/hephbuild/heph/sandbox"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/flock"
	fs2 "github.com/hephbuild/heph/utils/fs"
	"github.com/hephbuild/heph/utils/hash"
	"github.com/hephbuild/heph/utils/instance"
	"github.com/hephbuild/heph/utils/maps"
	"github.com/hephbuild/heph/utils/sets"
	"github.com/hephbuild/heph/utils/tar"
	"github.com/hephbuild/heph/vfssimple"
	"github.com/hephbuild/heph/worker"
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
	Observability     *observability.Observability
	GetFlowID         func() string
	Summary           *obsummary.Summary
	PlatformProviders []PlatformProvider
	RemoteCacheHints  *rcache.HintStore

	DisableNamedCacheWrite bool

	SourceFiles packages.SourceFiles
	Packages    *packages.Registry

	BuildFilesState *buildfiles.State

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
	Pool                       *worker.Pool

	exitHandlersm       sync.Mutex
	exitHandlers        []func(error)
	exitHandlersRunning bool

	gcLock                flock.Locker
	toolsLock             flock.Locker
	autocompleteCacheLock flock.Locker

	orderedCachesLock flock.Locker
	orderedCaches     []CacheConfig
	CloudClient       *cloudclient.HephClient
	CloudClientAuth   *cloudclient.HephClient
	CloudToken        string
}

type PlatformProvider struct {
	config.Platform
	platform.Provider
}

type Config struct {
	config.Config
	Profiles []string
	Caches   []CacheConfig
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

	pkgs := packages.NewRegistry(packages.Registry{
		RepoRoot:       root,
		HomeRoot:       homeDir,
		RootsCachePath: homeDir.Join("roots").Abs(),
	})

	return &Engine{
		Root:          root,
		HomeDir:       homeDir,
		LocalCache:    loc.(*vfsos.Location),
		Targets:       NewTargets(0),
		Observability: observability.NewTelemetry(),
		Packages:      pkgs,
		BuildFilesState: buildfiles.NewState(buildfiles.State{
			Packages: pkgs,
		}),
		RemoteCacheHints:      &rcache.HintStore{},
		cacheHashInput:        &maps.Map[string, string]{},
		cacheHashOutput:       &maps.Map[string, string]{},
		codegenPaths:          map[string]*Target{},
		tools:                 NewTargets(0),
		Labels:                sets.NewStringSet(0),
		gcLock:                flock.NewFlock("Global GC", homeDir.Join("tmp", "gc.lock").Abs()),
		toolsLock:             flock.NewFlock("Tools", homeDir.Join("tmp", "tools.lock").Abs()),
		autocompleteCacheLock: flock.NewFlock("Autocomplete cache", homeDir.Join("tmp", "ac_cache.lock").Abs()),
		orderedCachesLock:     flock.NewFlock("Order cache", homeDir.Join("tmp", "order_cache.lock").Abs()),
		dag:                   &DAG{dag.NewDAG()},
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

func (e *Engine) ScheduleTargetsWithDeps(ctx context.Context, targets []*Target, skip *Target) (*WaitGroupMap, error) {
	rrs := make([]TargetRunRequest, 0, len(targets))
	for _, target := range targets {
		rrs = append(rrs, TargetRunRequest{Target: target})
	}

	return e.ScheduleTargetRRsWithDeps(ctx, rrs, skip)
}

func ContextWithForegroundWaitGroup(ctx context.Context) (context.Context, *worker.WaitGroup) {
	deps := &worker.WaitGroup{}
	ctx = context.WithValue(ctx, "heph_pool_deps", deps)

	return ctx, deps
}

func ForegroundWaitGroup(ctx context.Context) *worker.WaitGroup {
	if deps, ok := ctx.Value("heph_pool_deps").(*worker.WaitGroup); ok {
		return deps
	}

	return nil
}

func (e *Engine) ScheduleTargetRRsWithDeps(ctx context.Context, rrs TargetRunRequests, skip *Target) (*WaitGroupMap, error) {
	return e.ScheduleV2TargetRRsWithDeps(ctx, rrs, skip)
}

func TargetStatus(t *Target, status string) observability.StatusFactory {
	return targetStatus{t.FQN, "", status}
}

type targetStatus struct {
	fqn, output, status string
}

var (
	targetColor = lipgloss.AdaptiveColor{Light: "#FFBB00", Dark: "#FFCA33"}
	targetStyle = struct {
		target, output lipgloss.Style
	}{
		lipgloss.NewStyle().Foreground(targetColor),
		lipgloss.NewStyle().Foreground(targetColor).Bold(true),
	}
)

func (t targetStatus) String(r *lipgloss.Renderer) string {
	target, output := targetStyle.target.Renderer(r), targetStyle.output.Renderer(r)

	outputStr := ""
	if t.output != "" {
		outputStr = output.Render("|" + t.output)
	}

	return target.Render(t.fqn) + outputStr + " " + t.status
}

func TargetOutputStatus(t *Target, output string, status string) observability.StatusFactory {
	if output == "" {
		output = "-"
	}
	return targetStatus{t.FQN, output, status}
}

func (e *Engine) ScheduleTargetRun(ctx context.Context, rr TargetRunRequest, deps *worker.WaitGroup) (*worker.Job, error) {
	j := e.Pool.Schedule(ctx, &worker.Job{
		Name: rr.Target.FQN,
		Deps: deps,
		Hook: WorkerStageFactory(func(job *worker.Job) (context.Context, *observability.TargetSpan) {
			return e.Observability.SpanRun(job.Ctx(), rr.Target.Target)
		}),
		Do: func(w *worker.Worker, ctx context.Context) error {
			e := NewTargetRunEngine(e)

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

func (e *Engine) collectNamedOutFromTar(ctx context.Context, target *Target, outputs []string) (*ActualOutNamedPaths, error) {
	tp := &ActualOutNamedPaths{}

	for name := range target.Out.Named() {
		if !ads.Contains(outputs, name) {
			continue
		}

		tp.ProvisionName(name)

		artifact := target.artifacts.OutTar(name)

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

func (e *Engine) collectOutFromArtifact(ctx context.Context, target *Target, artifact artifacts.Artifact) (fs2.Paths, error) {
	r, err := artifacts.UncompressedReaderFromArtifact(artifact, e.cacheDir(target).Abs())
	if err != nil {
		return nil, err
	}
	defer r.Close()

	files, err := tar.UntarList(ctx, r, e.tarListPath(artifact, target))
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
	ctx, span := e.Observability.SpanCollectOutput(ctx, target.Target)
	defer span.EndError(rerr)

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

func (e *Engine) populateActualFilesFromTar(ctx context.Context, target *Target, outputs []string) error {
	log.Tracef("populateActualFilesFromTar %v", target.FQN)

	target.actualOutFiles = &ActualOutNamedPaths{}
	target.actualSupportFiles = make(fs2.Paths, 0)

	var err error

	target.actualOutFiles, err = e.collectNamedOutFromTar(ctx, target, outputs)
	if err != nil {
		return fmt.Errorf("out: %w", err)
	}

	if target.HasSupportFiles {
		art := target.artifacts.OutTar(targetspec.SupportFilesOutput)

		target.actualSupportFiles, err = e.collectOutFromArtifact(ctx, target, art)
		if err != nil {
			return fmt.Errorf("support: %w", err)
		}
	}

	return nil
}

func (e *Engine) sandboxRoot(target *Target) fs2.Path {
	folder := "__target_" + target.Name
	if target.ConcurrentExecution {
		folder = "__target_tmp_" + instance.UID + "_" + target.Name
	}

	p := e.HomeDir.Join("sandbox", target.Package.Path, folder)

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
	cfg.Engine.GC = true
	cfg.Engine.CacheHints = true
	cfg.CacheOrder = config.CacheOrderLatency
	cfg.Platforms = map[string]config.Platform{
		"local": {
			Name:     "local",
			Provider: "local",
			Priority: 100,
		},
	}
	cfg.Watch.Ignore = append(cfg.Watch.Ignore, e.HomeDir.Join(".heph/**/*").Abs())

	err := config.ParseAndApply("/etc/.hephconfig", &cfg)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}

	err = config.ParseAndApply(e.Root.Join(".hephconfig").Abs(), &cfg)
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

	for name, cache := range cfg.Caches {
		uri := cache.URI
		if !strings.HasSuffix(uri, "/") {
			uri += "/"
		}
		loc, err := vfssimple.NewLocation(uri)
		if err != nil {
			return fmt.Errorf("cache %v :%w", name, err)
		}

		e.Config.Caches = append(e.Config.Caches, CacheConfig{
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

func (e *Engine) RegisterRemoveWithLocker(l flock.Locker, path string) {
	e.RegisterExitHandler(func() {
		ctx := context.Background()
		ok, err := l.TryLock(ctx)
		if err != nil {
			log.Errorf("lock rm %v: %v", path, err)
			return
		}
		if !ok {
			return
		}
		defer l.Unlock()

		err = os.RemoveAll(path)
		if err != nil {
			log.Error(err)
		}
	})
}

func (e *Engine) RegisterExitHandlerWithErr(f func(error)) {
	if e.exitHandlersRunning {
		return
	}

	e.exitHandlersm.Lock()
	defer e.exitHandlersm.Unlock()

	e.exitHandlers = append(e.exitHandlers, f)
}

func (e *Engine) RegisterExitHandler(f func()) {
	e.RegisterExitHandlerWithErr(func(error) {
		f()
	})
}

func (e *Engine) RunExitHandlers(err error) {
	e.exitHandlersm.Lock()
	defer e.exitHandlersm.Unlock()

	e.exitHandlersRunning = true
	defer func() {
		e.exitHandlersRunning = false
	}()

	for i := len(e.exitHandlers) - 1; i >= 0; i-- {
		h := e.exitHandlers[i]
		h(err)
	}

	e.exitHandlers = nil
}

func (e *Engine) GetCodegenOrigin(path string) (*Target, bool) {
	if dep, ok := e.codegenPaths[path]; ok {
		return dep, ok
	}

	for cpath, dep := range e.codegenPaths {
		if ok, _ := utils.PathMatchExactOrPrefixAny(path, cpath); ok {
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
