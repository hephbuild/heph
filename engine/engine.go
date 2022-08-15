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
	ranStatAn            bool
	codegenPaths         map[string]*Target
}

type Config struct {
	Profiles []string
	config.Config
	Cache []CacheConfig
}

type CacheConfig struct {
	config.Cache
	Name     string
	Read     bool
	Write    bool
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
		log.Tracef("hashinput %v took %v", target.FQN, time.Since(start))
	}()

	h := xxh3.New()

	for _, dep := range target.Tools {
		dh := e.hashOutput(dep.Target)

		h.Write([]byte(dep.Name))
		h.Write([]byte(dh))
	}

	for _, tool := range target.HostTools {
		h.Write([]byte(tool.Name))
	}

	for _, dep := range target.HashDeps.Targets {
		dh := e.hashOutput(dep)

		h.Write([]byte(dh))
	}

	for _, dep := range target.HashDeps.Files {
		err := e.hashFile(h, dep)
		if err != nil {
			panic(fmt.Errorf("%v: hashFile %v %w", target.FQN, dep.Abs(), err))
		}
	}

	for _, cmd := range target.Cmds {
		h.Write([]byte(cmd))
	}

	for _, file := range target.CachedFiles {
		h.Write([]byte(file.RelRoot()))
	}

	envEntries := make([]string, 0)
	for k, v := range target.Env {
		envEntries = append(envEntries, k+v)
	}

	sort.Strings(envEntries)

	for _, e := range envEntries {
		h.Write([]byte(e))
	}

	if target.Gen {
		h.Write([]byte{1})
	}

	hb := h.Sum128().Bytes()

	sh := hex.EncodeToString(hb[:])

	e.cacheHashInputMutex.Lock()
	e.cacheHashInput[cacheId] = sh
	e.cacheHashInputMutex.Unlock()

	return sh
}

func (e *Engine) hashOutput(target *Target) string {
	cacheId := target.FQN + "_" + e.hashInput(target)
	e.cacheHashOutputMutex.RLock()
	if h, ok := e.cacheHashOutput[cacheId]; ok {
		e.cacheHashOutputMutex.RUnlock()
		return h
	}
	e.cacheHashOutputMutex.RUnlock()

	start := time.Now()
	defer func() {
		log.Tracef("hashoutput %v took %v", target.FQN, time.Since(start))
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

	for _, file := range target.ActualFilesOut() {
		err := e.hashFile(h, file)
		if err != nil {
			panic(fmt.Errorf("%v: hashFile %v %w", target.FQN, file.Abs(), err))
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

func (e *Engine) ScheduleTarget(ctx context.Context, pool *worker.Pool, target *Target) error {
	ancestors, err := e.DAG().GetAncestors(target)
	if err != nil {
		return err
	}

	log.Tracef("Scheduling %v", target.FQN)

	pool.Schedule(&worker.Job{
		ID: target.FQN,
		Wait: func(ctx context.Context) {
			select {
			case <-ctx.Done():
			case <-ancestors.WaitAllRan():
			}
			return
		},
		Do: func(w *worker.Worker, ctx context.Context) error {
			e := TargetRunEngine{
				Engine:  e,
				Pool:    pool,
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

	return nil
}

func (e *Engine) ScheduleTargetDeps(ctx context.Context, pool *worker.Pool, target *Target) (Targets, error) {
	ancestors, err := e.DAG().GetAncestors(target)
	if err != nil {
		return nil, err
	}

	for _, target := range ancestors {
		err := e.ScheduleTarget(ctx, pool, target)
		if err != nil {
			return nil, err
		}
	}

	return ancestors, nil
}

func (e *Engine) collectOut(target *Target, files PackagePaths) (PackagePaths, error) {
	out := make(PackagePaths, 0)

	defer func() {
		sort.SliceStable(out, func(i, j int) bool {
			return out[i].RelRoot() < out[j].RelRoot()
		})
	}()

	if len(files) == 1 && files[0].Path == "*" {
		err := filepath.WalkDir(target.OutRoot.Abs, func(path string, d fs.DirEntry, err error) error {
			if d.IsDir() {
				return nil
			}

			relRoot, err := filepath.Rel(target.OutRoot.Abs, path)
			if err != nil {
				return err
			}

			out = append(out, PackagePath{
				Package: e.createPkg(""),
				Path:    relRoot,
				Root:    target.OutRoot.Abs,
			})

			return nil
		})
		if err != nil {
			return nil, fmt.Errorf("collect output *: %w", err)
		}

		return out, nil
	}

	for _, file := range files {
		file = file.WithRoot(target.OutRoot.Abs)

		abs := strings.HasPrefix(file.Path, "/")
		path := strings.TrimPrefix(file.Path, "/")
		pkg := target.Package
		if abs {
			pkg = e.createPkg("")
		}

		if strings.Contains(path, "*") {
			pattern := path
			if !abs {
				pattern = filepath.Join(pkg.Root.RelRoot, path)
			}

			err := utils.StarWalkAbs(target.OutRoot.Abs, pattern, nil, func(path string, d fs.DirEntry, err error) error {
				relPkg, err := filepath.Rel(filepath.Join(target.OutRoot.Abs, pkg.Root.RelRoot), path)
				if err != nil {
					return err
				}

				out = append(out, PackagePath{
					Package: pkg,
					Path:    relPkg,
					Root:    target.OutRoot.Abs,
				})

				return nil
			})
			if err != nil {
				return nil, fmt.Errorf("collect output %v: %w", file.Path, err)
			}

			return out, nil
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

				relPkg, err := filepath.Rel(filepath.Join(target.OutRoot.Abs, pkg.Root.RelRoot), path)
				if err != nil {
					return err
				}

				out = append(out, PackagePath{
					Package: pkg,
					Path:    relPkg,
					Root:    target.OutRoot.Abs,
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

	target.actualFilesOut = make([]PackagePath, 0)
	target.actualcachedFiles = make([]PackagePath, 0)

	if empty {
		return nil
	}

	target.actualcachedFiles, err = e.collectOut(target, target.CachedFiles)
	if err != nil {
		return fmt.Errorf("cached: %w", err)
	}

	target.actualFilesOut, err = e.collectOut(target, target.Out)
	if err != nil {
		return fmt.Errorf("out: %w", err)
	}

	return nil
}

func (e *Engine) sandboxRoot(target *Target) string { // TODO: PackagePath
	return filepath.Join(e.HomeDir, "sandbox", target.Package.FullName, target.Name)
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
	err := os.RemoveAll(target.runLock.Path())
	if err != nil {
		return err
	}

	err = os.RemoveAll(target.cacheLock.Path())
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

	mainCfg, err := config.Parse(filepath.Join(e.Root, ".hephconfig"))
	if err != nil {
		return err
	}

	localCfg, err := config.Parse(filepath.Join(e.Root, ".hephconfig.local"))
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}

	cfg := mainCfg.Merge(localCfg)

	for _, profile := range e.Config.Profiles {
		profileCfg, err := config.Parse(filepath.Join(e.Root, ".hephconfig."+profile))
		if err != nil && !errors.Is(err, os.ErrNotExist) {
			return err
		}

		cfg = cfg.Merge(profileCfg)
	}

	// Finalize config

	cfg.BuildFiles.Ignore = append(cfg.BuildFiles.Ignore, ".heph")

	for k, cache := range cfg.Cache {
		if cache.Read == nil {
			v := true
			cache.Read = &v
		}
		if cache.Write == nil {
			v := false
			cache.Write = &v
		}
		cfg.Cache[k] = cache
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
			Read:     *cache.Read,
			Write:    *cache.Write,
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

func (e *Engine) runGenerated(target *Target) error {
	log.Tracef("run generated %v", target.FQN)

	start := time.Now()
	defer func() {
		log.Tracef("runGenerated %v took %v", target.FQN, time.Since(start))
	}()

	targets := make(Targets, 0)

	files := target.ActualFilesOut()

	for _, file := range files {
		re := &runBuildEngine{
			Engine: e,
			pkg:    e.createPkg(filepath.Dir(file.RelRoot())),
			registerTarget: func(spec TargetSpec) error {
				e.TargetsLock.Lock()
				defer e.TargetsLock.Unlock()

				if t := e.Targets.Find(spec.FQN); t != nil {
					// TODO handle already registered
					return nil
				}

				t := &Target{
					TargetSpec: spec,
				}

				targets = append(targets, t)
				e.Targets = append(e.Targets, t)

				return nil
			},
		}

		_, err := re.runBuildFile(file.Abs())
		if err != nil {
			return err
		}
	}

	log.Tracef("run generated got %v targets", len(targets))

	for _, t := range targets {
		err := e.processTarget(t)
		if err != nil {
			return fmt.Errorf("process: %v: %w", t.FQN, err)
		}
	}

	return nil
}
