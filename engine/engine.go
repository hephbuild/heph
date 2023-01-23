package engine

import (
	tar2 "archive/tar"
	"context"
	"errors"
	"fmt"
	"github.com/c2fo/vfs/v6"
	vfsos "github.com/c2fo/vfs/v6/backend/os"
	"github.com/charmbracelet/lipgloss"
	"go.opentelemetry.io/otel/trace"
	"go.starlark.net/starlark"
	"heph/config"
	"heph/engine/htrace"
	log "heph/hlog"
	"heph/packages"
	"heph/sandbox"
	"heph/targetspec"
	"heph/utils"
	"heph/utils/flock"
	fs2 "heph/utils/fs"
	"heph/utils/hash"
	"heph/utils/maps"
	"heph/utils/sets"
	"heph/utils/tar"
	"heph/vfssimple"
	"heph/worker"
	"io"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"syscall"
	"time"
)

type Engine struct {
	Cwd        string
	Root       fs2.Path
	HomeDir    fs2.Path
	Config     Config
	LocalCache *vfsos.Location
	Stats      *htrace.Stats
	Tracer     trace.Tracer
	RootSpan   trace.Span

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

	log.Tracef("home dir %v", homeDir)

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
	}
}

func (e *Engine) DAG() *DAG {
	return e.dag
}

func (e *Engine) CodegenPaths() map[string]*Target {
	return e.codegenPaths
}

func (e *Engine) hashFile(h hash.Hash, file fs2.Path) error {
	return e.hashFilePath(h, file.Abs())
}

func (e *Engine) hashDepsTargets(h hash.Hash, targets []TargetWithOutput) {
	for _, dep := range targets {
		if len(dep.Target.Out.Names()) == 0 {
			continue
		}

		h.String(dep.Target.FQN)
		dh := e.hashOutput(dep.Target, dep.Output)
		h.String(dh)

		if dep.Mode != targetspec.TargetSpecDepModeCopy {
			h.String(string(dep.Mode))
		}
	}
}

func (e *Engine) hashFiles(h hash.Hash, hashMethod string, files fs2.Paths) {
	for _, dep := range files {
		h.String(dep.RelRoot())

		switch hashMethod {
		case targetspec.HashFileContent:
			err := e.hashFile(h, dep)
			if err != nil {
				panic(fmt.Errorf("hashDeps: hashFile %v %w", dep.Abs(), err))
			}
		case targetspec.HashFileModTime:
			err := e.hashFileModTime(h, dep)
			if err != nil {
				panic(fmt.Errorf("hashDeps: hashFileModTime %v %w", dep.Abs(), err))
			}
		default:
			panic(fmt.Sprintf("unhandled hash_input: %v", hashMethod))
		}
	}
}

var copyBufPool = sync.Pool{
	New: func() interface{} {
		return make([]byte, 1000)
	},
}

func (e *Engine) hashFilePath(h hash.Hash, path string) error {
	info, err := os.Lstat(path)
	if err != nil {
		return fmt.Errorf("stat: %w", err)
	}

	if info.Mode().Type() == os.ModeSymlink {
		link, err := os.Readlink(path)
		if err != nil {
			return err
		}

		h.String(link)
		return nil
	}

	f, err := os.Open(path)
	if err != nil {
		return fmt.Errorf("open: %w", err)
	}
	defer f.Close()

	return e.hashFileReader(h, info, f)
}

func (e *Engine) hashFilePerm(h hash.Hash, m os.FileMode) {
	// TODO: figure out a way to properly hash file permission, taking into account different umask
	//h.UI32(uint32(m.Perm()))
}

func (e *Engine) hashFileReader(h hash.Hash, info os.FileInfo, f io.Reader) error {
	e.hashFilePerm(h, info.Mode())

	buf := copyBufPool.Get().([]byte)
	defer copyBufPool.Put(buf)

	_, err := io.CopyBuffer(h, f, buf)
	if err != nil {
		return fmt.Errorf("copy: %w", err)
	}

	return nil
}

func (e *Engine) hashTar(h hash.Hash, tarPath string) error {
	return tar.Walk(context.Background(), tarPath, func(hdr *tar2.Header, r *tar2.Reader) error {
		h.String(hdr.Name)

		switch hdr.Typeflag {
		case tar2.TypeReg:
			err := e.hashFileReader(h, hdr.FileInfo(), io.LimitReader(r, hdr.Size))
			if err != nil {
				return err
			}

			return nil
		case tar2.TypeSymlink:
			e.hashFilePerm(h, hdr.FileInfo().Mode())
			h.String(hdr.Linkname)
		case tar2.TypeDir:
			return nil
		default:
			return fmt.Errorf("untar: unsupported type %v", hdr.Typeflag)
		}

		return nil
	})
}

func (e *Engine) hashFileModTime(h hash.Hash, file fs2.Path) error {
	return e.hashFileModTimePath(h, file.Abs())
}

func (e *Engine) hashFileModTimePath(h hash.Hash, path string) error {
	info, err := os.Lstat(path)
	if err != nil {
		return fmt.Errorf("stat: %w", err)
	}

	if info.Mode().Type() == os.ModeSymlink {
		return fmt.Errorf("symlink cannot be hashed")
	}

	e.hashFilePerm(h, info.Mode())
	h.I64(info.ModTime().UnixNano())

	return nil
}

func (e *Engine) ResetCacheHashInput(target *Target) {
	ks := make([]string, 0)

	for _, k := range e.cacheHashInput.Keys() {
		if strings.HasPrefix(k, target.FQN) {
			ks = append(ks, k)
		}
	}

	for _, k := range ks {
		e.cacheHashInput.Delete(k)
	}
}

func (e *Engine) hashInputFiles(h hash.Hash, target *Target) error {
	e.hashFiles(h, targetspec.HashFileModTime, target.Deps.All().Files)

	for _, dep := range target.Deps.All().Targets {
		err := e.hashInputFiles(h, dep.Target)
		if ErrStopWalk != nil {
			return err
		}
	}

	if target.DifferentHashDeps {
		e.hashFiles(h, targetspec.HashFileModTime, target.HashDeps.Files)

		for _, dep := range target.HashDeps.Targets {
			err := e.hashInputFiles(h, dep.Target)
			if ErrStopWalk != nil {
				return err
			}
		}
	}

	return nil
}

func hashCacheId(target *Target) string {
	idh := hash.NewHash()
	for _, fqn := range target.linkingDeps.FQNs() {
		idh.String(fqn)
	}

	return target.FQN + idh.Sum()
}

func (e *Engine) HashInput(target *Target) string {
	return e.hashInput(target)
}

func (e *Engine) hashInput(target *Target) string {
	mu := e.cacheHashInputTargetMutex.Get(target.FQN)
	mu.Lock()
	defer mu.Unlock()

	cacheId := hashCacheId(target)

	if h, ok := e.cacheHashInput.GetOk(cacheId); ok {
		return h
	}

	start := time.Now()
	defer func() {
		log.Debugf("hashinput %v took %v", target.FQN, time.Since(start))
	}()

	h := hash.NewDebuggableHash(target.FQN + "_hash_input")
	h.I64(8) // Force break all caches

	h.String("=")
	for _, dep := range target.Tools.Targets {
		h.String(dep.Name)

		dh := e.hashOutput(dep.Target, dep.Output)
		h.String(dh)
	}

	h.String("=")
	for _, tool := range target.Tools.Hosts {
		h.String(tool.Name)
	}

	h.String("=")
	if target.DifferentHashDeps {
		h.String("=")
		e.hashDepsTargets(h, target.HashDeps.Targets)
		e.hashFiles(h, target.HashFile, target.HashDeps.Files)
	} else {
		h.String("=")
		for _, name := range target.Deps.Names() {
			h.String("=")
			h.String(name)

			deps := target.Deps.Name(name)

			e.hashDepsTargets(h, deps.Targets)
			e.hashFiles(h, target.HashFile, deps.Files)
		}
	}

	h.String("=")
	for _, cmd := range target.Run {
		h.String(cmd)
	}
	h.String(target.Executor)

	if target.IsTextFile() {
		h.String("=")
		h.Write(target.FileContent)
	}

	h.String("=")
	hash.HashArray(h, target.TargetSpec.Out, func(file targetspec.TargetSpecOutFile) string {
		return file.Name + file.Path
	})

	if target.OutInSandbox {
		h.Bool(target.OutInSandbox)
	}

	h.String("=")
	hash.HashMap(h, target.Env, func(k, v string) string {
		return k + v
	})

	h.String("=")
	h.Bool(target.Gen)

	h.String("=")
	h.String(target.SrcEnv.All)
	hash.HashMap(h, target.SrcEnv.Named, func(k, v string) string {
		return k + v
	})
	h.String(target.OutEnv)

	sh := h.Sum()

	e.cacheHashInput.Set(cacheId, sh)

	return sh
}
func (e *Engine) HashOutput(target *Target, output string) string {
	return e.hashOutput(target, output)
}

func (e *Engine) hashOutput(target *Target, output string) string {
	mu := e.cacheHashOutputTargetMutex.Get(target.FQN + "|" + output)
	mu.Lock()
	defer mu.Unlock()

	hashInput := e.hashInput(target)
	cacheId := target.FQN + "|" + output + "_" + hashInput

	if h, ok := e.cacheHashOutput.GetOk(cacheId); ok {
		return h
	}

	start := time.Now()
	defer func() {
		log.Debugf("hashoutput %v|%v took %v", target.FQN, output, time.Since(start))
	}()

	if !target.OutWithSupport.HasName(output) {
		panic(fmt.Sprintf("%v does not output `%v`", target, output))
	}

	file := e.targetOutputHashFile(target, output)
	b, err := os.ReadFile(file)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		log.Errorf("reading %v: %v", file, err)
	}

	if sh := strings.TrimSpace(string(b)); len(sh) > 0 {
		e.cacheHashOutput.Set(cacheId, sh)

		return sh
	}

	// Sanity check, will bomb if not called in the right order
	_ = target.ActualOutFiles()

	h := hash.NewDebuggableHash(target.FQN + "_hash_out_" + output)

	h.String(output)

	tarPath := e.targetOutputTarFile(target, output)
	err = e.hashTar(h, tarPath)
	if err != nil {
		panic(fmt.Errorf("hashOutput: %v: hashTar %v %w", target.FQN, tarPath, err))
	}

	if target.HasSupportFiles && output != targetspec.SupportFilesOutput {
		sh := e.hashOutput(target, targetspec.SupportFilesOutput)
		h.String(sh)
	}

	sh := h.Sum()

	e.cacheHashOutput.Set(cacheId, sh)

	return sh
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
				e := TargetRunEngine{
					Engine: e,
					Status: w.Status,
				}

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
	return TargetOutputStatus(t, "", status)
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
	return targetStatus{t.FQN, output, status}
}

func (e *Engine) ScheduleTargetPostRunOrWarm(ctx context.Context, target *Target, deps *worker.WaitGroup, outputs []string) *worker.Job {
	return e.Pool.Schedule(ctx, &worker.Job{
		Name: "postrunwarm " + target.FQN,
		Deps: deps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			e := TargetRunEngine{
				Engine: e,
				Status: w.Status,
			}

			return e.postRunOrWarm(ctx, target, outputs)
		},
	})
}

func (e *Engine) ScheduleTarget(ctx context.Context, rr TargetRunRequest, deps *worker.WaitGroup) (*worker.Job, error) {
	j := e.Pool.Schedule(ctx, &worker.Job{
		Name: rr.Target.FQN,
		Deps: deps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			e := TargetRunEngine{
				Engine: e,
				Status: w.Status,
			}

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

func (e *Engine) collectNamedOut(target *Target, namedPaths *OutNamedPaths, root string) (*ActualOutNamedPaths, error) {
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

func (e *Engine) collectNamedOutFromTar(target *Target, namedPaths *OutNamedPaths) (*ActualOutNamedPaths, error) {
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
	return e.HomeDir.Join("sandbox", target.Package.FullName, "__target_"+target.Name)
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
		target := target
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

func (e *Engine) GetTargetShortcuts() []targetspec.TargetSpec {
	aliases := make([]targetspec.TargetSpec, 0)
	for _, target := range e.Targets.Slice() {
		spec := target.TargetSpec

		if spec.Package.FullName == "" {
			aliases = append(aliases, spec)
		}
	}
	return aliases
}

func (e *Engine) GetFileDeps(targets ...*Target) []fs2.Path {
	return e.getFileDeps(targets, func(target *Target) TargetDeps {
		return target.Deps.All()
	})
}

func (e *Engine) GetFileHashDeps(targets ...*Target) []fs2.Path {
	return e.getFileDeps(targets, func(target *Target) TargetDeps {
		return target.HashDeps
	})
}

func (e *Engine) getFileDeps(targets []*Target, f func(*Target) TargetDeps) []fs2.Path {
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
