package engine

import (
	tar2 "archive/tar"
	"context"
	"errors"
	"fmt"
	"github.com/bmatcuk/doublestar/v4"
	"github.com/c2fo/vfs/v6"
	vfsos "github.com/c2fo/vfs/v6/backend/os"
	log "github.com/sirupsen/logrus"
	"go.opentelemetry.io/otel/trace"
	"go.starlark.net/starlark"
	"heph/config"
	"heph/engine/htrace"
	"heph/packages"
	"heph/sandbox"
	"heph/targetspec"
	"heph/utils"
	"heph/utils/flock"
	fs2 "heph/utils/fs"
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
	Cwd           string
	Root          fs2.Path
	HomeDir       fs2.Path
	Config        Config
	LocalCache    *vfsos.Location
	TraceRecorder *htrace.Stats
	Tracer        trace.Tracer
	RootSpan      trace.Span

	DisableNamedCache bool

	SourceFiles   packages.SourceFiles
	packagesMutex sync.Mutex
	Packages      map[string]*packages.Package

	TargetsLock sync.Mutex
	Targets     *Targets
	Labels      []string

	Params map[string]string

	dag *DAG

	autocompleteHash string

	cacheHashInputTargetMutex  utils.KMutex
	cacheHashInputMutex        sync.RWMutex
	cacheHashInput             map[string]string
	cacheHashOutputTargetMutex utils.KMutex
	cacheHashOutputMutex       sync.RWMutex
	cacheHashOutput            map[string]string // TODO: LRU
	RanGenPass                 bool
	RanInit                    bool
	codegenPaths               map[string]*Target
	fetchRootCache             map[string]fs2.Path
	cacheRunBuildFilem         sync.RWMutex
	cacheRunBuildFile          map[string]starlark.StringDict
	Pool                       *worker.Pool

	exitHandlersm       sync.Mutex
	exitHandlers        []func()
	exitHandlersRunning bool

	gcLock *flock.Flock
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
		TraceRecorder:     &htrace.Stats{},
		Tracer:            trace.NewNoopTracerProvider().Tracer(""),
		Packages:          map[string]*packages.Package{},
		cacheHashInput:    map[string]string{},
		cacheHashOutput:   map[string]string{},
		codegenPaths:      map[string]*Target{},
		fetchRootCache:    map[string]fs2.Path{},
		cacheRunBuildFile: map[string]starlark.StringDict{},
		gcLock:            flock.NewFlock(homeDir.Join("tmp", "gc.lock").Abs()),
	}
}

func (e *Engine) DAG() *DAG {
	return e.dag
}

func (e *Engine) CodegenPaths() map[string]*Target {
	return e.codegenPaths
}

func (e *Engine) hashFile(h utils.Hash, file fs2.Path) error {
	return e.hashFilePath(h, file.Abs())
}

func (e *Engine) hashDepsTargets(h utils.Hash, targets []TargetWithOutput) {
	for _, dep := range targets {
		if len(dep.Target.Out.All()) == 0 {
			continue
		}

		h.String(dep.Target.FQN)
		dh := e.hashOutput(dep.Target, dep.Output)
		h.String(dh)
	}
}

func (e *Engine) hashFiles(h utils.Hash, hashMethod string, files fs2.Paths) {
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

func (e *Engine) hashFilePath(h utils.Hash, path string) error {
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

func (e *Engine) hashFileReader(h utils.Hash, info os.FileInfo, f io.Reader) error {
	// We only really care if the file is executable
	// https://stackoverflow.com/a/60128480/3212099
	h.UI32(uint32(info.Mode().Perm() & 0111))

	buf := copyBufPool.Get().([]byte)
	defer copyBufPool.Put(buf)

	_, err := io.CopyBuffer(h, f, buf)
	if err != nil {
		return fmt.Errorf("copy: %w", err)
	}

	return nil
}

func (e *Engine) hashFileModTime(h utils.Hash, file fs2.Path) error {
	return e.hashFileModTimePath(h, file.Abs())
}

func (e *Engine) hashFileModTimePath(h utils.Hash, path string) error {
	info, err := os.Lstat(path)
	if err != nil {
		return fmt.Errorf("stat: %w", err)
	}

	if info.Mode().Type() == os.ModeSymlink {
		return fmt.Errorf("symlink cannot be hashed")
	}

	h.UI32(uint32(info.Mode().Perm()))
	h.I64(info.ModTime().UnixNano())

	return nil
}

func (e *Engine) GetHashInputCache() map[string]string {
	return e.cacheHashInput
}

func (e *Engine) ResetCacheHashInput(target *Target) {
	e.cacheHashInputMutex.Lock()
	defer e.cacheHashInputMutex.Unlock()

	delete(e.cacheHashInput, hashCacheId(target))
}

func (e *Engine) hashInputFiles(h utils.Hash, target *Target) error {
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
	idh := utils.NewHash()
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

	e.cacheHashInputMutex.RLock()
	if h, ok := e.cacheHashInput[cacheId]; ok {
		e.cacheHashInputMutex.RUnlock()
		return h
	}
	e.cacheHashInputMutex.RUnlock()

	start := time.Now()
	defer func() {
		log.Debugf("hashinput %v took %v", target.FQN, time.Since(start))
	}()

	h := utils.NewHash()
	h.I64(4) // Force break all caches

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
	utils.HashArray(h, target.TargetSpec.Out, func(file targetspec.TargetSpecOutFile) string {
		return file.Name + file.Path
	})

	if target.OutInSandbox {
		h.Bool(target.OutInSandbox)
	}

	h.String("=")
	for _, file := range target.SupportFiles {
		h.String(file.RelRoot())
	}

	h.String("=")
	utils.HashMap(h, target.Env, func(k, v string) string {
		return k + v
	})

	h.String("=")
	h.Bool(target.Gen)

	h.String("=")
	h.String(target.SrcEnv.All)
	for k, v := range target.SrcEnv.Named {
		h.String(k + v)
	}
	h.String(target.OutEnv)

	if target.Timeout > 0 {
		h.String("=")
		h.I64(target.Timeout.Nanoseconds())
	}

	sh := h.Sum()

	e.cacheHashInputMutex.Lock()
	e.cacheHashInput[cacheId] = sh
	e.cacheHashInputMutex.Unlock()

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

	e.cacheHashOutputMutex.RLock()
	if h, ok := e.cacheHashOutput[cacheId]; ok {
		e.cacheHashOutputMutex.RUnlock()
		return h
	}
	e.cacheHashOutputMutex.RUnlock()

	start := time.Now()
	defer func() {
		log.Debugf("hashoutput %v took %v", target.FQN, time.Since(start))
	}()

	if !target.Out.HasName(output) {
		panic(fmt.Sprintf("%v does not output `%v`", target, output))
	}

	file := e.targetOutputHashFile(target, output)
	b, err := os.ReadFile(file)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		log.Errorf("reading %v: %v", file, err)
	}

	if sh := strings.TrimSpace(string(b)); len(sh) > 0 {
		e.cacheHashOutputMutex.Lock()
		e.cacheHashOutput[cacheId] = sh
		e.cacheHashOutputMutex.Unlock()

		return sh
	}

	h := utils.NewHash()

	h.String(output)

	// Sanity check, will bomb if not called in the right order
	_ = target.ActualOutFiles()

	tarPath := e.targetOutputTarFile(target, output)

	err = tar.Walk(context.Background(), tarPath, func(hdr *tar2.Header, r *tar2.Reader) error {
		h.String(hdr.Name)

		switch hdr.Typeflag {
		case tar2.TypeReg:
			h.I64(hdr.Mode)

			err := e.hashFileReader(h, hdr.FileInfo(), io.LimitReader(r, hdr.Size))
			if err != nil {
				return err
			}

			return nil
		case tar2.TypeDir:
			return nil
		case tar2.TypeSymlink:
			h.String(hdr.Linkname)
		default:
			return fmt.Errorf("untar: unsupported type %v", hdr.Typeflag)
		}

		return nil
	})
	if err != nil {
		panic(fmt.Errorf("hashOutput: %v: hashTar %v %w", target.FQN, tarPath, err))
	}

	sh := h.Sum()

	e.cacheHashOutputMutex.Lock()
	e.cacheHashOutput[cacheId] = sh
	e.cacheHashOutputMutex.Unlock()

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

func (e *Engine) ScheduleTargetRRsWithDeps(octx context.Context, rrs TargetRunRequests, skip *Target) (_ *WaitGroupMap, rerr error) {
	targets := rrs.Targets()

	sctx, span := e.SpanScheduleTargetWithDeps(octx, targets)
	defer func() {
		if rerr != nil {
			span.RecordError(rerr)
			span.End()
		}
	}()

	toAssess, err := e.DAG().GetOrderedAncestors(targets, true)
	if err != nil {
		return nil, err
	}

	needRun := NewTargets(0)
	needCacheWarm := NewTargets(0)

	for _, target := range targets {
		if skip != nil && skip.FQN == target.FQN {
			parents, err := e.DAG().GetParents(target)
			if err != nil {
				return nil, err
			}

			needCacheWarm.AddAll(parents)
			needRun.Add(target)
		} else {
			needCacheWarm.Add(target)
		}
	}

	deps := &WaitGroupMap{}
	pullMetaDeps := &WaitGroupMap{}
	pullAllMetaDeps := &worker.WaitGroup{}

	for _, target := range toAssess {
		target := target

		pj := e.Pool.Schedule(sctx, &worker.Job{
			Name: "pull_meta " + target.FQN,
			Deps: pullMetaDeps.Get(target.FQN),
			Do: func(w *worker.Worker, ctx context.Context) error {
				w.Status(fmt.Sprintf("Scheduling analysis %v...", target.FQN))

				parents, err := e.DAG().GetParents(target)
				if err != nil {
					return err
				}

				hasParentCacheMiss := false
				for _, parent := range parents {
					if needRun.Find(parent.FQN) != nil {
						hasParentCacheMiss = true
						break
					}
				}

				rr := rrs.Get(target)
				if !hasParentCacheMiss && target.Cache.Enabled && !rr.NoCache {
					w.Status(fmt.Sprintf("Pulling meta %v...", target.FQN))

					e := TargetRunEngine{
						Engine:  e,
						Print:   w.Status,
						Context: ctx,
					}

					cached, err := e.PullTargetMeta(ctx, target)
					if err != nil {
						return err
					}

					if cached {
						return nil
					}
				}

				needCacheWarm.AddAll(parents)
				needRun.Add(target)

				return nil
			},
		})
		pullAllMetaDeps.Add(pj)

		children, err := e.DAG().GetChildren(target)
		if err != nil {
			return nil, err
		}

		for _, child := range children {
			pullMetaDeps.Get(child.FQN).Add(pj)
		}
	}

	scheduleDeps := &WaitGroupMap{}

	for _, target := range toAssess {
		target := target

		targetDeps := deps.Get(target.FQN)
		targetDeps.AddSem()

		sdeps := &worker.WaitGroup{}
		// TODO: Replace with waiting for all dependants pull meta of target.FQN in the list of all ancestors
		sdeps.AddChild(pullAllMetaDeps)
		sdeps.AddChild(scheduleDeps.Get(target.FQN))

		sj := e.Pool.Schedule(octx, &worker.Job{
			Name: "schedule " + target.FQN,
			Deps: sdeps,
			Do: func(w *worker.Worker, ctx context.Context) error {
				w.Status(fmt.Sprintf("Scheduling %v...", target.FQN))

				parents, err := e.DAG().GetParents(target)
				if err != nil {
					return err
				}

				wdeps := &worker.WaitGroup{}
				for _, parent := range parents {
					pdeps := deps.Get(parent.FQN)
					wdeps.AddChild(pdeps)
				}

				if skip != nil && target.FQN == skip.FQN {
					log.Debugf("%v skip", target.FQN)
					targetDeps.AddChild(wdeps)

					return nil
				}

				if needRun.Find(target.FQN) != nil {
					j, err := e.ScheduleTarget(ctx, rrs.Get(target), wdeps)
					if err != nil {
						return err
					}
					targetDeps.Add(j)
				} else if needCacheWarm.Find(target.FQN) != nil {
					j, err := e.ScheduleTargetCacheWarm(ctx, target, wdeps)
					if err != nil {
						return err
					}
					targetDeps.Add(j)
				}

				log.Debugf("%v nothing to do", target.FQN)

				return nil
			},
		})

		children, err := e.DAG().GetChildren(target)
		if err != nil {
			return nil, err
		}

		for _, child := range children {
			scheduleDeps.Get(child.FQN).Add(sj)
		}

		targetDeps.Add(sj)
		targetDeps.DoneSem()
	}

	go func() {
		<-deps.All().Done()
		span.End()
	}()

	return deps, nil
}

func (e *Engine) ScheduleTargetCacheWarm(ctx context.Context, target *Target, deps *worker.WaitGroup) (*worker.Job, error) {
	j := e.Pool.Schedule(ctx, &worker.Job{
		Name: "warm " + target.FQN,
		Deps: deps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			e := TargetRunEngine{
				Engine:  e,
				Print:   w.Status,
				Context: ctx,
			}

			w.Status(fmt.Sprintf("Fetching cache %v...", target.FQN))

			cached, err := e.WarmTargetCache(ctx, target)
			if err != nil {
				return err
			}

			if !cached {
				return fmt.Errorf("%v expected cache pull to succeed", target.FQN)
			}

			return nil
		},
	})

	return j, nil
}

func (e *Engine) ScheduleTarget(ctx context.Context, rr TargetRunRequest, deps *worker.WaitGroup) (*worker.Job, error) {
	j := e.Pool.Schedule(ctx, &worker.Job{
		Name: rr.Target.FQN,
		Deps: deps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			e := TargetRunEngine{
				Engine:  e,
				Print:   w.Status,
				Context: ctx,
			}

			err := e.Run(rr, sandbox.IOConfig{})
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
		tp.ProvisonName(name)

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
		tp.ProvisonName(name)

		files, err := e.collectOutFromTar(target, e.targetOutputTarFile(target, name))
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
	err := target.tarLock.Lock()
	if err != nil {
		return nil, err
	}

	defer func() {
		err := target.tarLock.Unlock()
		if err != nil {
			log.Error(err)
		}
	}()

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
	out := make(fs2.Paths, 0)

	for _, file := range files {
		pattern := file.RelRoot()

		if !utils.IsGlob(pattern) && !fs2.PathExists(filepath.Join(root, pattern)) {
			return nil, fmt.Errorf("%v did not output %v", target.FQN, file.RelRoot())
		}

		err := utils.StarWalk(root, pattern, nil, func(path string, d fs.DirEntry, err error) error {
			out = append(out, fs2.NewPath(root, path))

			return nil
		})
		if err != nil {
			return nil, fmt.Errorf("collect output %v: %w", file.RelRoot(), err)
		}
	}

	out.Sort()

	return out, nil
}

func (e *TargetRunEngine) populateActualFiles(ctx context.Context, target *Target, outRoot string) (rerr error) {
	span := e.SpanCollectOutput(ctx, target)
	defer func() {
		span.RecordError(rerr)
		span.End()
	}()

	empty, err := fs2.IsDirEmpty(outRoot)
	if err != nil {
		return fmt.Errorf("collect output: isempty: %w", err)
	}

	target.actualOutFiles = &ActualOutNamedPaths{}
	target.actualSupportFiles = make(fs2.Paths, 0)

	if empty {
		return nil
	}

	target.actualSupportFiles, err = e.collectOut(target, target.SupportFiles, outRoot)
	if err != nil {
		return fmt.Errorf("cached: %w", err)
	}

	target.actualOutFiles, err = e.collectNamedOut(target, target.Out, outRoot)
	if err != nil {
		return fmt.Errorf("out: %w", err)
	}

	return nil
}

func (e *Engine) populateActualFilesFromTar(target *Target) error {
	target.actualOutFiles = &ActualOutNamedPaths{}
	target.actualSupportFiles = make(fs2.Paths, 0)

	var err error
	if len(target.SupportFiles) > 0 {
		target.actualSupportFiles, err = e.collectOutFromTar(target, e.targetSupportTarFile(target))
		if err != nil {
			return fmt.Errorf("cached: %w", err)
		}
	}

	target.actualOutFiles, err = e.collectNamedOutFromTar(target, target.Out)
	if err != nil {
		return fmt.Errorf("out: %w", err)
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

	err = target.cacheLock.Clean()
	if err != nil {
		return err
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
	cfg.BuildFiles.Ignore = append(cfg.BuildFiles.Ignore, "/.heph")
	cfg.CacheHistory = 3

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
	for _, l := range e.Labels {
		if l == label {
			return true
		}
	}

	return false
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
		if !e.HasLabel(label) {
			e.Labels = append(e.Labels, label)
		}
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
				match, err := doublestar.PathMatch(file.RelRoot(), path)
				if err != nil {
					return nil, err
				}

				if match {
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
