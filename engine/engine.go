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

	Context context.Context

	SourceFiles   SourceFiles
	packagesMutex sync.Mutex
	Packages      map[string]*Package

	TargetsLock sync.Mutex
	Targets     *Targets
	Labels      []string

	dag *DAG

	cacheHashInputMutex  sync.RWMutex
	cacheHashInput       map[string]string
	cacheHashOutputMutex sync.RWMutex
	cacheHashOutput      map[string]string
	ranGenPass           bool
	codegenPaths         map[string]*Target
	Pool                 *worker.Pool
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

func New(rootPath string, ctx context.Context) *Engine {
	root := Path{Root: rootPath}

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
		Context:         ctx,
		LocalCache:      loc.(*vfsos.Location),
		Targets:         NewTargets(0),
		Packages:        map[string]*Package{},
		cacheHashInput:  map[string]string{},
		cacheHashOutput: map[string]string{},
		codegenPaths:    map[string]*Target{},
	}
}

func (e *Engine) DAG() *DAG {
	return e.dag
}

func (e *Engine) CodegenPaths() map[string]*Target {
	return e.codegenPaths
}

func (e *Engine) hashFile(h utils.Hash, file PackagePath) error {
	return e.hashFilePath(h, file.Abs())
}

func (e *Engine) hashDepsTargets(h utils.Hash, targets []TargetWithOutput) {
	for _, dep := range targets {
		dh := e.hashOutput(dep.Target, dep.Output)

		h.String(dh)
	}
}
func (e *Engine) hashDepsFiles(h utils.Hash, target *Target, files []PackagePath) {
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

	_, err = io.Copy(h, f)
	if err != nil {
		return fmt.Errorf("copy: %w", err)
	}

	return nil
}

func (e *Engine) hashFileModTime(h utils.Hash, file PackagePath) error {
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

func (e *Engine) hashInput(target *Target) string {
	cacheId := target.FQN
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
	for _, dep := range target.Tools {
		dh := e.hashOutput(dep.Target, "")

		h.String(dep.Name)
		h.String(dh)
	}

	h.String("=")
	for _, tool := range target.HostTools {
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

	h.String("=")
	outEntries := make([]string, 0)
	for _, file := range target.TargetSpec.Out {
		outEntries = append(outEntries, file.Name+file.Path)
	}

	sort.Strings(outEntries)

	for _, entry := range outEntries {
		h.String(entry)
	}

	h.String("=")
	for _, file := range target.CachedFiles {
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
	cacheId := target.FQN + "_" + e.hashInput(target)
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

	file := e.cacheDir(target, e.hashInput(target)).Join(outputHashFile).Abs()
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

func (e *Engine) ScheduleTargetsWithDeps(targets []*Target, skip *Target) (map[*Target]*worker.WaitGroup, error) {
	ancestors, err := e.DAG().GetOrderedAncestors(targets)
	if err != nil {
		return nil, err
	}

	needRun := NewTargets(0)
	needCacheWarm := NewTargets(0)

	toAssess := ancestors
	toAssess = append(toAssess, targets...)

	deps := map[*Target]*worker.WaitGroup{}

	pullMetadeps := &worker.WaitGroup{}

	for _, target := range targets {
		if skip != nil && skip.FQN == target.FQN {
			parents, err := e.DAG().GetParents(target)
			if err != nil {
				return nil, err
			}

			for _, parent := range parents {
				needCacheWarm.Add(parent)
			}
			needRun.Add(target)
		} else {
			needCacheWarm.Add(target)
		}
	}

	schedId := fmt.Sprintf("_%v", time.Now().Nanosecond())

	for _, target := range toAssess {
		target := target

		parents, err := e.DAG().GetParents(target)
		if err != nil {
			return nil, err
		}

		pj := e.Pool.Schedule(&worker.Job{
			ID: "pull_meta_" + target.FQN + schedId,
			Deps: jobs(parents, e.Pool, func(target *Target) string {
				return "pull_meta_" + target.FQN + schedId
			}),
			Do: func(w *worker.Worker, ctx context.Context) error {
				w.Status(fmt.Sprintf("Scheduling analysis %v...", target.FQN))

				hasParentCacheMiss := false
				for _, parent := range parents {
					if needRun.Find(parent.FQN) != nil {
						hasParentCacheMiss = true
						break
					}
				}

				if !hasParentCacheMiss && target.ShouldCache {
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

				for _, parent := range parents {
					needCacheWarm.Add(parent)
				}
				needRun.Add(target)

				return nil
			},
		})

		deps[target] = &worker.WaitGroup{}
		pullMetadeps.Add(pj)
	}

	for _, target := range toAssess {
		target := target

		targetDeps := deps[target]
		targetDeps.AddSem()

		parents, err := e.DAG().GetParents(target)
		if err != nil {
			return nil, err
		}

		scheduleDeps := jobs(parents, e.Pool, func(target *Target) string {
			return "schedule_" + target.FQN + schedId
		})
		scheduleDeps.AddChild(pullMetadeps)

		sj := e.Pool.Schedule(&worker.Job{
			ID:   "schedule_" + target.FQN + schedId,
			Deps: scheduleDeps,
			Do: func(w *worker.Worker, ctx context.Context) error {
				w.Status(fmt.Sprintf("Scheduling %v...", target.FQN))

				wdeps := &worker.WaitGroup{}
				for _, parent := range parents {
					pdeps := deps[parent]
					wdeps.AddChild(pdeps)
				}

				if skip != nil && target.FQN == skip.FQN {
					log.Debugf("%v skip", target.FQN)
					targetDeps.AddChild(wdeps)

					return nil
				}

				if needRun.Find(target.FQN) != nil {
					j, err := e.ScheduleTarget(target, wdeps)
					if err != nil {
						return err
					}
					targetDeps.Add(j)
				} else if needCacheWarm.Find(target.FQN) != nil {
					j, err := e.ScheduleTargetCacheWarm(target, wdeps)
					if err != nil {
						return err
					}
					targetDeps.Add(j)
				}

				log.Debugf("%v nothing to do", target.FQN)

				return nil
			},
		})
		targetDeps.Add(sj)
		targetDeps.DoneSem()
	}

	return deps, nil
}

func (e *Engine) ScheduleTargetCacheWarm(target *Target, deps *worker.WaitGroup) (*worker.Job, error) {
	j := e.Pool.Schedule(&worker.Job{
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

func (e *Engine) ScheduleTarget(target *Target, deps *worker.WaitGroup) (*worker.Job, error) {
	j := e.Pool.Schedule(&worker.Job{
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

func jobs(targets []*Target, pool *worker.Pool, idProvider func(*Target) string) *worker.WaitGroup {
	deps := &worker.WaitGroup{}

	for _, target := range targets {
		j := pool.Job(idProvider(target))
		if j == nil {
			panic(fmt.Sprintf("job is nil for %v", target.FQN))
		}

		deps.Add(j)
	}

	return deps
}

func (e *Engine) collectNamedOutFromActualFiles(target *Target, namedPaths *TargetNamedPackagePath) (*TargetNamedPackagePath, error) {
	tp := &TargetNamedPackagePath{}

	for _, filePath := range target.actualcachedFiles {
		for name, paths := range namedPaths.Named() {
			for _, file := range paths {
				file = file.WithRoot(target.OutRoot.Abs()).WithPackagePath(true)

				abs := strings.HasPrefix(file.Path, "/")
				path := strings.TrimPrefix(file.Path, "/")
				pkg := target.Package
				if abs {
					pkg = e.createPkg("")
				}
				pattern := filepath.Join(pkg.FullName, path)

				relRoot := filePath.RelRoot()

				match, err := doublestar.PathMatch(pattern, relRoot)
				if err != nil {
					return nil, err
				}

				if match {
					relPkg, err := filepath.Rel(target.OutRoot.Join(pkg.FullName).Abs(), filePath.Abs())
					if err != nil {
						return nil, err
					}

					tp.Add(name, PackagePath{
						Package:     pkg,
						Path:        relPkg,
						Root:        target.OutRoot.Abs(),
						PackagePath: file.PackagePath,
					})
				}
			}
		}
	}

	tp.Sort()

	return tp, nil
}

func (e *Engine) collectNamedOut(target *Target, namedPaths *TargetNamedPackagePath) (*TargetNamedPackagePath, error) {
	tp := &TargetNamedPackagePath{}

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

func (e *Engine) collectOut(target *Target, files PackagePaths) (PackagePaths, error) {
	out := make(PackagePaths, 0)

	defer func() {
		sort.SliceStable(out, func(i, j int) bool {
			return out[i].RelRoot() < out[j].RelRoot()
		})
	}()

	for _, file := range files {
		file = file.WithRoot(target.OutRoot.Abs()).WithPackagePath(true)

		abs := strings.HasPrefix(file.Path, "/")
		path := strings.TrimPrefix(file.Path, "/")
		pkg := target.Package
		if abs {
			pkg = e.createPkg("")
		}

		if strings.Contains(path, "*") {
			pattern := path
			if !abs {
				pattern = filepath.Join(pkg.FullName, path)
			}

			err := utils.StarWalkAbs(target.OutRoot.Abs(), pattern, nil, func(path string, d fs.DirEntry, err error) error {
				relPkg, err := filepath.Rel(target.OutRoot.Join(pkg.FullName).Abs(), path)
				if err != nil {
					return err
				}

				out = append(out, PackagePath{
					Package:     pkg,
					Path:        relPkg,
					Root:        target.OutRoot.Abs(),
					PackagePath: file.PackagePath,
				})

				return nil
			})
			if err != nil {
				return nil, fmt.Errorf("collect output %v: %w", file.Path, err)
			}

			continue
		}

		f, err := os.Stat(file.Abs())
		if err != nil {
			return nil, err
		}

		if f.IsDir() {
			err := filepath.WalkDir(file.Abs(), func(path string, d fs.DirEntry, err error) error {
				if d.IsDir() {
					return nil
				}

				relPkg, err := filepath.Rel(target.OutRoot.Join(pkg.FullName).Abs(), path)
				if err != nil {
					return err
				}

				out = append(out, PackagePath{
					Package:     pkg,
					Path:        relPkg,
					Root:        target.OutRoot.Abs(),
					PackagePath: file.PackagePath,
				})

				return nil
			})
			if err != nil {
				return nil, fmt.Errorf("collect output: %v %w", file.Path, err)
			}
		} else {
			if !utils.PathExists(file.Abs()) {
				return nil, fmt.Errorf("%v: %w", file.Abs(), fs.ErrNotExist)
			}
			out = append(out, file)
		}
	}

	return out, nil
}

func (e *Engine) populateActualFiles(target *Target) (err error) {
	empty, err := utils.IsDirEmpty(target.OutRoot.Abs())
	if err != nil {
		return fmt.Errorf("collect output: isempty: %w", err)
	}

	target.actualFilesOut = &TargetNamedPackagePath{}
	target.actualcachedFiles = make(PackagePaths, 0)

	if empty {
		return nil
	}

	target.actualcachedFiles, err = e.collectOut(target, target.CachedFiles)
	if err != nil {
		return fmt.Errorf("cached: %w", err)
	}

	collector := e.collectNamedOutFromActualFiles
	if !target.ShouldCache {
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

func (e *Engine) GetTargetShortcuts() []*Target {
	aliases := make([]*Target, 0)
	for _, target := range e.Targets.Slice() {
		if target.Package.FullName == "" {
			aliases = append(aliases, target)
		}
	}
	return aliases
}
