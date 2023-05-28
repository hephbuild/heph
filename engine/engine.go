package engine

import (
	"context"
	"fmt"
	"github.com/charmbracelet/lipgloss"
	"github.com/hephbuild/heph/engine/artifacts"
	"github.com/hephbuild/heph/engine/buildfiles"
	"github.com/hephbuild/heph/engine/graph"
	"github.com/hephbuild/heph/engine/hroot"
	"github.com/hephbuild/heph/engine/observability"
	"github.com/hephbuild/heph/engine/status"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/packages"
	"github.com/hephbuild/heph/platform"
	"github.com/hephbuild/heph/rcache"
	"github.com/hephbuild/heph/sandbox"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/finalizers"
	"github.com/hephbuild/heph/utils/flock"
	fs2 "github.com/hephbuild/heph/utils/fs"
	"github.com/hephbuild/heph/utils/instance"
	"github.com/hephbuild/heph/utils/sets"
	"github.com/hephbuild/heph/utils/tar"
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
	Root              *hroot.State
	Config            *graph.Config
	Observability     *observability.Observability
	GetFlowID         func() string
	PlatformProviders []platform.PlatformProvider
	LocalCache        *LocalCacheState
	RemoteCacheHints  *rcache.HintStore
	Packages          *packages.Registry
	BuildFilesState   *buildfiles.State
	Graph             *graph.State
	Pool              *worker.Pool
	Finalizers        *finalizers.Finalizers

	DisableNamedCacheWrite bool

	toolsLock             flock.Locker
	autocompleteCacheLock flock.Locker

	orderedCachesLock flock.Locker
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
			WorkdirRoot:      fs2.Path{},
			SandboxRoot:      fs2.Path{},
			OutExpansionRoot: nil, // Set during execution
			runLock:          nil, // Set after
			postRunWarmLock:  e.lockFactory(gtarget, "postrunwarm"),
			cacheLocks:       nil, // Set after
		}
		if t.ConcurrentExecution {
			t.runLock = flock.NewMutex(t.FQN)
		} else {
			t.runLock = e.lockFactory(t, "run")
		}

		t.cacheLocks = make(map[string]flock.Locker, len(t.Artifacts.All()))
		for _, artifact := range t.Artifacts.All() {
			t.cacheLocks[artifact.Name()] = e.lockFactory(t, "cache_"+artifact.Name())
		}

		t.SandboxRoot = e.sandboxRoot(t).Join("_dir")

		t.WorkdirRoot = e.Root.Root
		if t.Sandbox {
			t.WorkdirRoot = t.SandboxRoot
		}

		t.cacheLocks = make(map[string]flock.Locker, len(t.Artifacts.All()))
		for _, artifact := range t.Artifacts.All() {
			t.cacheLocks[artifact.Name()] = e.lockFactory(t, "cache_"+artifact.Name())
		}

		return t
	})
	e.toolsLock = flock.NewFlock("Tools", e.Root.Home.Join("tmp", "tools.lock").Abs())
	e.autocompleteCacheLock = flock.NewFlock("Autocomplete cache", e.Root.Home.Join("tmp", "ac_cache.lock").Abs())
	e.orderedCachesLock = flock.NewFlock("Order cache", e.Root.Home.Join("tmp", "order_cache.lock").Abs())
	return &e
}

func (e *Engine) lockFactory(t targetspec.Specer, resource string) flock.Locker {
	ts := t.Spec()
	p := e.lockPath(t, resource)

	return flock.NewFlock(ts.FQN+" ("+resource+")", p)
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

func (e *Engine) ScheduleTargetsWithDeps(ctx context.Context, targets []*graph.Target, skip []targetspec.Specer) (*WaitGroupMap, error) {
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

func (e *Engine) ScheduleTargetRRsWithDeps(ctx context.Context, rrs TargetRunRequests, skip []targetspec.Specer) (*WaitGroupMap, error) {
	return e.ScheduleV2TargetRRsWithDeps(ctx, rrs, skip)
}

func TargetStatus(t targetspec.Specer, status string) status.Statuser {
	return targetStatus{t.Spec().FQN, "", status}
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

func TargetOutputStatus(t *Target, output string, status string) status.Statuser {
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

func (e *Engine) populateActualFiles(ctx context.Context, target *Target, outRoot string) (rerr error) {
	ctx, span := e.Observability.SpanCollectOutput(ctx, target.Target.Target)
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
		art := target.Artifacts.OutTar(targetspec.SupportFilesOutput)

		target.actualSupportFiles, err = e.collectOutFromArtifact(ctx, target, art)
		if err != nil {
			return fmt.Errorf("support: %w", err)
		}
	}

	return nil
}

func (e *Engine) sandboxRoot(specer targetspec.Specer) fs2.Path {
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

func (e *Engine) Clean(async bool) error {
	return deleteDir(e.Root.Home.Abs(), async)
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
