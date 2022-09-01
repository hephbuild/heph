package engine

import (
	"context"
	"encoding/hex"
	"errors"
	"fmt"
	"github.com/c2fo/vfs/v6"
	vfsos "github.com/c2fo/vfs/v6/backend/os"
	log "github.com/sirupsen/logrus"
	"github.com/zeebo/xxh3"
	"hash"
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

type SourceFile struct {
	Path string
}

type SourceFiles []*SourceFile

func (sf SourceFiles) Find(p string) *SourceFile {
	for _, file := range sf {
		if file.Path == p {
			return file
		}
	}

	return nil
}

type Engine struct {
	Cwd        string
	Root       string
	HomeDir    string
	Config     Config
	LocalCache *vfsos.Location

	Context context.Context

	SourceFiles   SourceFiles
	packagesMutex sync.Mutex
	Packages      map[string]*Package

	TargetsLock sync.Mutex
	Targets     Targets
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

func New(root string) *Engine {
	homeDir := filepath.Join(root, ".heph")

	log.Tracef("home dir %v", homeDir)

	err := os.MkdirAll(homeDir, os.ModePerm)
	if err != nil {
		log.Fatal(fmt.Errorf("create homedir %v: %w", homeDir, err))
	}

	cacheDir := filepath.Join(homeDir, "cache")

	loc, err := vfssimple.NewLocation("file://" + cacheDir + "/")
	if err != nil {
		log.Fatal(fmt.Errorf("cache location: %w", err))
	}

	return &Engine{
		Root:            root,
		HomeDir:         homeDir,
		Context:         context.Background(),
		LocalCache:      loc.(*vfsos.Location),
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

func (e *Engine) hashFile(h hash.Hash, file PackagePath) error {
	info, err := os.Lstat(file.Abs())
	if err != nil {
		return fmt.Errorf("stat: %w", err)
	}

	if info.Mode().Type() == os.ModeSymlink {
		return fmt.Errorf("symlink cannot be hashed")
	}

	h.Write([]byte(fmt.Sprint(info.Mode().Perm())))

	f, err := os.Open(file.Abs())
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

func (e *Engine) hashFileModTime(h hash.Hash, file PackagePath) error {
	info, err := os.Lstat(file.Abs())
	if err != nil {
		return fmt.Errorf("stat: %w", err)
	}

	if info.Mode().Type() == os.ModeSymlink {
		return fmt.Errorf("symlink cannot be hashed")
	}

	h.Write([]byte(fmt.Sprint(info.Mode().Perm())))
	h.Write([]byte(fmt.Sprint(info.ModTime())))

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

	h := xxh3.New()

	h.WriteString("=")
	for _, dep := range target.Tools {
		dh := e.hashOutput(dep.Target, "")

		h.Write([]byte(dep.Name))
		h.Write([]byte(dh))
	}

	h.WriteString("=")
	for _, tool := range target.HostTools {
		h.Write([]byte(tool.Name))
	}

	h.WriteString("=")
	for _, dep := range target.HashDeps.Targets {
		dh := e.hashOutput(dep.Target, dep.Output)

		h.Write([]byte(dh))
	}

	h.WriteString("=")
	for _, dep := range target.HashDeps.Files {
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

	h.WriteString("=")
	for _, cmd := range target.Run {
		h.Write([]byte(cmd))
	}
	h.Write([]byte(target.Executor))

	h.WriteString("=")
	outEntries := make([]string, 0)
	for _, file := range target.TargetSpec.Out {
		outEntries = append(outEntries, file.Name+file.Path)
	}

	sort.Strings(outEntries)

	for _, entry := range outEntries {
		h.WriteString(entry)
	}

	h.WriteString("=")
	for _, file := range target.CachedFiles {
		h.Write([]byte(file.RelRoot()))
	}

	h.WriteString("=")
	envEntries := make([]string, 0)
	for k, v := range target.Env {
		envEntries = append(envEntries, k+v)
	}

	sort.Strings(envEntries)

	for _, e := range envEntries {
		h.Write([]byte(e))
	}

	h.WriteString("=")
	if target.Gen {
		h.Write([]byte{1})
	} else {
		h.Write([]byte{0})
	}

	h.WriteString("=")
	h.WriteString(target.SrcEnv)
	h.WriteString(target.OutEnv)

	hb := h.Sum128().Bytes()

	sh := hex.EncodeToString(hb[:])

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

	file := filepath.Join(e.cacheDir(target, e.hashInput(target)), outputHashFile)
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

	h := xxh3.New()

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

	hb := h.Sum128().Bytes()

	sh := hex.EncodeToString(hb[:])

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

func (e *Engine) ScheduleTarget(target *Target) (*worker.Job, error) {
	parents, err := e.DAG().GetParents(target)
	if err != nil {
		return nil, err
	}

	j := e.Pool.Schedule(&worker.Job{
		ID:   target.FQN,
		Deps: jobs(parents, e.Pool),
		Do: func(w *worker.Worker, ctx context.Context) error {
			e := TargetRunEngine{
				Engine:  e,
				Pool:    e.Pool,
				Worker:  w,
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

func jobs(targets []*Target, pool *worker.Pool) *worker.WaitGroup {
	deps := &worker.WaitGroup{}

	for _, target := range targets {
		j := pool.Job(target.FQN)
		if j == nil {
			panic(fmt.Sprintf("job is nil for %v", target.FQN))
		}

		deps.Add(j)
	}

	return deps
}

func (e *Engine) ScheduleTargetDeps(target *Target) (*worker.WaitGroup, error) {
	parents, err := e.DAG().GetParents(target)
	if err != nil {
		return nil, err
	}

	deps := &worker.WaitGroup{}

	for _, parent := range parents {
		_, err := e.ScheduleTargetDeps(parent)
		if err != nil {
			return nil, err
		}

		j, err := e.ScheduleTarget(parent)
		if err != nil {
			return nil, err
		}

		deps.Add(j)
	}

	return deps, nil
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
		file = file.WithRoot(target.OutRoot.Abs).WithPackagePath(true)

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

			err := utils.StarWalkAbs(target.OutRoot.Abs, pattern, nil, func(path string, d fs.DirEntry, err error) error {
				relPkg, err := filepath.Rel(filepath.Join(target.OutRoot.Abs, pkg.FullName), path)
				if err != nil {
					return err
				}

				out = append(out, PackagePath{
					Package:     pkg,
					Path:        relPkg,
					Root:        target.OutRoot.Abs,
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

				relPkg, err := filepath.Rel(filepath.Join(target.OutRoot.Abs, pkg.FullName), path)
				if err != nil {
					return err
				}

				out = append(out, PackagePath{
					Package:     pkg,
					Path:        relPkg,
					Root:        target.OutRoot.Abs,
					PackagePath: file.PackagePath,
				})

				return nil
			})
			if err != nil {
				return nil, fmt.Errorf("collect output: %v %w", file.Path, err)
			}
		} else {
			out = append(out, file)
		}
	}

	return out, nil
}

func (e *Engine) populateActualFiles(target *Target) (err error) {
	empty, err := utils.IsDirEmpty(target.OutRoot.Abs)
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

	target.actualFilesOut, err = e.collectNamedOut(target, target.Out)
	if err != nil {
		return fmt.Errorf("out: %w", err)
	}

	return nil
}

func (e *Engine) sandboxRoot(target *Target) string { // TODO: PackagePath
	return filepath.Join(e.HomeDir, "sandbox", target.Package.FullName, "__target_"+target.Name)
}

func (e *Engine) Clean(async bool) error {
	return deleteDir(e.HomeDir, async)
}

func (e *Engine) CleanTarget(target *Target, async bool) error {
	sandboxDir := e.sandboxRoot(target)
	err := deleteDir(sandboxDir, async)
	if err != nil {
		return err
	}

	cacheDir := e.cacheDir(target, "")
	err = deleteDir(cacheDir, async)
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

	err := config.ParseAndApply(filepath.Join(e.Root, ".hephconfig"), &cfg)
	if err != nil {
		return err
	}

	err = config.ParseAndApply(filepath.Join(e.Root, ".hephconfig.local"), &cfg)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}

	for _, profile := range e.Config.Profiles {
		err := config.ParseAndApply(filepath.Join(e.Root, ".hephconfig."+profile), &cfg)
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

func (e *Engine) GeneratedTargets() Targets {
	targets := make(Targets, 0)

	for _, target := range e.Targets {
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
	for _, target := range e.Targets {
		if target.Package.FullName == "" {
			aliases = append(aliases, target)
		}
	}
	return aliases
}
