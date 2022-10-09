package engine

import (
	"context"
	"errors"
	"fmt"
	"github.com/bmatcuk/doublestar/v4"
	"github.com/c2fo/vfs/v6"
	vfsos "github.com/c2fo/vfs/v6/backend/os"
	log "github.com/sirupsen/logrus"
	"heph/config"
	"heph/sandbox"
	"heph/utils"
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
	Root       Path
	HomeDir    Path
	Config     Config
	LocalCache *vfsos.Location

	DisableNamedCache bool

	SourceFiles   SourceFiles
	packagesMutex sync.Mutex
	Packages      map[string]*Package

	TargetsLock sync.Mutex
	Targets     *Targets
	Labels      []string

	Params map[string]string

	dag *DAG

	cacheHashInputTargetMutex  utils.KMutex
	cacheHashInputMutex        sync.RWMutex
	cacheHashInput             map[string]string
	cacheHashOutputTargetMutex utils.KMutex
	cacheHashOutputMutex       sync.RWMutex
	cacheHashOutput            map[string]string // TODO: LRU
	ranGenPass                 bool
	codegenPaths               map[string]*Target
	fetchRootCache             map[string]Path
	Pool                       *worker.Pool
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

func New(rootPath string) *Engine {
	root := Path{root: rootPath}

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
		Root:            root,
		HomeDir:         homeDir,
		LocalCache:      loc.(*vfsos.Location),
		Targets:         NewTargets(0),
		Packages:        map[string]*Package{},
		cacheHashInput:  map[string]string{},
		cacheHashOutput: map[string]string{},
		codegenPaths:    map[string]*Target{},
		fetchRootCache:  map[string]Path{},
	}
}

func (e *Engine) DAG() *DAG {
	return e.dag
}

func (e *Engine) CodegenPaths() map[string]*Target {
	return e.codegenPaths
}

func (e *Engine) hashFile(h utils.Hash, file Path) error {
	return e.hashFilePath(h, file.Abs())
}

func (e *Engine) hashDepsTargets(h utils.Hash, targets []TargetWithOutput) {
	for _, dep := range targets {
		dh := e.hashOutput(dep.Target, dep.Output)

		h.String(dh)
	}
}
func (e *Engine) hashDepsFiles(h utils.Hash, target *Target, files Paths) {
	for _, dep := range files {
		h.String(dep.RelRoot())

		switch target.HashFile {
		case HashFileContent:
			err := e.hashFile(h, dep)
			if err != nil {
				panic(fmt.Errorf("hashDeps: %v: hashFile %v %w", target.FQN, dep.Abs(), err))
			}
		case HashFileModTime:
			err := e.hashFileModTime(h, dep)
			if err != nil {
				panic(fmt.Errorf("hashDeps: %v: hashFileModTime %v %w", target.FQN, dep.Abs(), err))
			}
		default:
			panic(fmt.Sprintf("unhandled hash_input: %v", target.HashFile))
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
		return fmt.Errorf("symlink cannot be hashed")
	}

	h.UI32(uint32(info.Mode().Perm()))

	f, err := os.Open(path)
	if err != nil {
		return fmt.Errorf("open: %w", err)
	}
	defer f.Close()

	buf := copyBufPool.Get().([]byte)
	defer copyBufPool.Put(buf)

	_, err = io.CopyBuffer(h, f, buf)
	if err != nil {
		return fmt.Errorf("copy: %w", err)
	}

	return nil
}

func (e *Engine) hashFileModTime(h utils.Hash, file Path) error {
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

	delete(e.cacheHashInput, target.FQN)
}

func (e *Engine) hashInput(target *Target) string {
	mu := e.cacheHashInputTargetMutex.Get(target.FQN)
	mu.Lock()
	defer mu.Unlock()

	idh := utils.NewHash()
	for _, fqn := range target.linkingDeps.FQNs() {
		idh.String(fqn)
	}

	cacheId := target.FQN + idh.Sum()

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

	h.String("=")
	for _, dep := range target.Tools.Targets {
		dh := e.hashOutput(dep.Target, "")

		h.String(dep.Name)
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
		e.hashDepsFiles(h, target, target.HashDeps.Files)
	} else {
		h.String("=")
		for _, name := range target.Deps.Names() {
			h.String("=")
			h.String(name)

			deps := target.Deps.Name(name)

			e.hashDepsTargets(h, deps.Targets)
			e.hashDepsFiles(h, target, deps.Files)
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
	outEntries := make([]string, 0)
	for _, file := range target.TargetSpec.Out {
		outEntries = append(outEntries, file.Name+file.Path)
	}

	sort.Strings(outEntries)

	for _, entry := range outEntries {
		h.String(entry)
	}

	if target.OutInSandbox {
		h.Bool(target.OutInSandbox)
	}

	h.String("=")
	for _, file := range target.CacheFiles {
		h.String(file.RelRoot())
	}

	h.String("=")
	envEntries := make([]string, 0)
	for k, v := range target.Env {
		envEntries = append(envEntries, k+v)
	}

	sort.Strings(envEntries)

	for _, e := range envEntries {
		h.String(e)
	}

	h.String("=")
	h.Bool(target.Gen)

	h.String("=")
	h.String(target.SrcEnv)
	h.String(target.OutEnv)

	sh := h.Sum()

	e.cacheHashInputMutex.Lock()
	e.cacheHashInput[cacheId] = sh
	e.cacheHashInputMutex.Unlock()

	return sh
}

func (e *Engine) hashOutput(target *Target, output string) string {
	mu := e.cacheHashOutputTargetMutex.Get(target.FQN)
	mu.Lock()
	defer mu.Unlock()

	hashInput := e.hashInput(target)
	cacheId := target.FQN + "_" + hashInput

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

	file := e.cacheDir(target, hashInput).Join(outputHashFile).Abs()
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

	actualOut := target.NamedActualFilesOut().All()
	if output != "" {
		actualOut = target.NamedActualFilesOut().Name(output)
	}

	for _, file := range actualOut {
		err := e.hashFile(h, file)
		if err != nil {
			panic(fmt.Errorf("actualOut: %v: hashFile %v %w", target.FQN, file.Abs(), err))
		}
	}

	sh := h.Sum()

	e.cacheHashOutputMutex.Lock()
	e.cacheHashOutput[cacheId] = sh
	e.cacheHashOutputMutex.Unlock()

	return sh
}

type TargetFailedError struct {
	Target *Target
	Err    error
}

func (t TargetFailedError) Error() string {
	return t.Err.Error()
}

func (t TargetFailedError) Is(target error) bool {
	_, ok := target.(TargetFailedError)

	return ok
}

type WaitGroupMap struct {
	mu sync.Mutex
	m  map[string]*worker.WaitGroup
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
	ancestors, err := e.DAG().GetOrderedAncestors(targets, false)
	if err != nil {
		return nil, err
	}

	needRun := NewTargets(0)
	needCacheWarm := NewTargets(0)

	toAssess := ancestors
	toAssess = append(toAssess, targets...)

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

	schedId := fmt.Sprintf("_%v", time.Now().Nanosecond())

	deps := &WaitGroupMap{}
	pullMetaDeps := &WaitGroupMap{}
	pullAllMetaDeps := &worker.WaitGroup{}

	for _, target := range toAssess {
		target := target

		pj := e.Pool.Schedule(ctx, &worker.Job{
			ID:   "pull_meta_" + target.FQN + schedId,
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

				if !hasParentCacheMiss && target.Cache.Enabled {
					w.Status(fmt.Sprintf("Pulling meta %v...", target.FQN))

					e := TargetRunEngine{
						Engine:  e,
						Print:   w.Status,
						Context: ctx,
					}

					cached, err := e.PullTargetMeta(target)
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

		sj := e.Pool.Schedule(ctx, &worker.Job{
			ID:   "schedule_" + target.FQN + schedId,
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
					j, err := e.ScheduleTarget(ctx, target, wdeps)
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

	return deps, nil
}

func (e *Engine) ScheduleTargetCacheWarm(ctx context.Context, target *Target, deps *worker.WaitGroup) (*worker.Job, error) {
	j := e.Pool.Schedule(ctx, &worker.Job{
		ID:   "warm_" + target.FQN,
		Deps: deps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			e := TargetRunEngine{
				Engine:  e,
				Print:   w.Status,
				Context: ctx,
			}

			w.Status(fmt.Sprintf("Fetching cache %v...", target.FQN))

			cached, err := e.WarmTargetCache(target)
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

func (e *Engine) ScheduleTarget(ctx context.Context, target *Target, deps *worker.WaitGroup) (*worker.Job, error) {
	j := e.Pool.Schedule(ctx, &worker.Job{
		ID:   target.FQN,
		Deps: deps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			e := TargetRunEngine{
				Engine:  e,
				Print:   w.Status,
				Context: ctx,
			}

			err := e.Run(target, sandbox.IOConfig{})
			if err != nil {
				return TargetFailedError{
					Target: target,
					Err:    err,
				}
			}

			return nil
		},
	})

	return j, nil
}

func (e *Engine) collectNamedOutFromActualFiles(target *Target, outNamedPaths *OutNamedPaths) (*ActualOutNamedPaths, error) {
	tp := &ActualOutNamedPaths{}

	for _, filePath := range target.actualcachedFiles {
		for name, opaths := range outNamedPaths.Named() {
			for _, opath := range opaths {
				path := opath.RelRoot()
				if strings.HasPrefix(opath.RelRoot(), "/") {
					path = strings.TrimPrefix(path, "/")
				}
				pattern := path

				relRoot := filePath.RelRoot()

				match, err := doublestar.PathMatch(pattern, relRoot)
				if err != nil {
					return nil, err
				}

				if match {
					tp.Add(name, filePath)
				}
			}
		}
	}

	tp.Sort()

	return tp, nil
}

func (e *Engine) collectNamedOut(target *Target, namedPaths *OutNamedPaths) (*ActualOutNamedPaths, error) {
	tp := &ActualOutNamedPaths{}

	for name, paths := range namedPaths.Named() {
		files, err := e.collectOut(target, paths)
		if err != nil {
			return nil, err
		}

		for _, file := range files {
			tp.Add(name, file)
		}
	}

	return tp, nil
}

func (e *Engine) collectOut(target *Target, files RelPaths) (Paths, error) {
	out := make(Paths, 0)

	defer func() {
		sort.SliceStable(out, func(i, j int) bool {
			return out[i].RelRoot() < out[j].RelRoot()
		})
	}()

	for _, file := range files {
		pattern := file.RelRoot()

		file := file.WithRoot(target.OutRoot.Abs())

		err := utils.StarWalk(file.root, pattern, nil, func(path string, d fs.DirEntry, err error) error {
			out = append(out, Path{
				root:    file.root,
				relRoot: path,
			})

			return nil
		})
		if err != nil {
			return nil, fmt.Errorf("collect output %v: %w", file.RelRoot(), err)
		}
	}

	return out, nil
}

func (e *Engine) populateActualFiles(target *Target) (err error) {
	empty, err := utils.IsDirEmpty(target.OutRoot.Abs())
	if err != nil {
		return fmt.Errorf("collect output: isempty: %w", err)
	}

	target.actualFilesOut = &ActualOutNamedPaths{}
	target.actualcachedFiles = make(Paths, 0)

	if empty {
		return nil
	}

	target.actualcachedFiles, err = e.collectOut(target, target.CacheFiles)
	if err != nil {
		return fmt.Errorf("cached: %w", err)
	}

	collector := e.collectNamedOutFromActualFiles
	if !target.Cache.Enabled {
		collector = e.collectNamedOut
	}

	target.actualFilesOut, err = collector(target, target.Out)
	if err != nil {
		return fmt.Errorf("out: %w", err)
	}

	return nil
}

func (e *Engine) sandboxRoot(target *Target) Path {
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

	cacheDir := e.cacheDir(target, "")
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
	} else if !utils.PathExists(dir) {
		return nil // not an error, just don't need to do anything.
	}

	log.Tracef("Deleting %v", dir)

	if async {
		newDir := utils.RandPath(os.TempDir(), filepath.Base(dir), "")

		err = os.Rename(dir, newDir)
		if err != nil {
			// May be because os.TempDir() and the current dir aren't on the same device, try a sibling folder
			newDir = utils.RandPath(filepath.Dir(dir), filepath.Base(dir), "")

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

func (e *Engine) GetTargetShortcuts() []TargetSpec {
	aliases := make([]TargetSpec, 0)
	for _, target := range e.Targets.Slice() {
		spec := target.TargetSpec

		if spec.Package.FullName == "" {
			aliases = append(aliases, spec)
		}
	}
	return aliases
}

func (e *Engine) GetFileDeps(targets ...*Target) []Path {
	filesm := map[string]Path{}
	for _, target := range targets {
		for _, file := range target.HashDeps.Files {
			filesm[file.Abs()] = file
		}
	}

	files := make([]Path, 0, len(filesm))
	for _, file := range filesm {
		files = append(files, file)
	}

	sort.SliceStable(files, func(i, j int) bool {
		return files[i].RelRoot() < files[j].RelRoot()
	})

	return files
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
