package engine

import (
	"errors"
	"fmt"
	"github.com/goccy/go-json"
	"go.starlark.net/resolve"
	"go.starlark.net/starlark"
	log "heph/hlog"
	"heph/packages"
	"heph/targetspec"
	"heph/utils"
	fs2 "heph/utils/fs"
	"heph/utils/hash"
	"io/fs"
	"os"
	"path"
	"path/filepath"
	"sort"
	"strings"
	"time"
)

func init() {
	resolve.AllowGlobalReassign = true
	resolve.AllowRecursion = true
}

const buildFilesPattern = "**/{BUILD,BUILD.*}"

func (e *Engine) collectBuildFiles(root string) (packages.SourceFiles, error) {
	files := make(packages.SourceFiles, 0)

	walkStartTime := time.Now()
	err := utils.StarWalk(root, buildFilesPattern, e.Config.BuildFiles.Ignore, func(path string, d fs.DirEntry, err error) error {
		if d.IsDir() {
			return nil
		}

		files = append(files, &packages.SourceFile{
			Path: filepath.Join(root, path),
		})
		return nil
	})
	if err != nil {
		return nil, err
	}
	log.Debugf("RunBuildFiles:walk took %v", time.Now().Sub(walkStartTime))

	return files, err
}

func (e *Engine) runBuildFiles(root string, pkgProvider func(dir string) *packages.Package) error {
	files, err := e.collectBuildFiles(root)
	if err != nil {
		return err
	}

	pkgs := make([]*packages.Package, 0)
	for _, file := range files {
		relRoot, err := filepath.Rel(e.Root.Abs(), filepath.Dir(file.Path))
		if err != nil {
			panic(err)
		}

		if relRoot == "." {
			relRoot = ""
		}

		pkg := pkgProvider(relRoot)

		pkgs = append(pkgs, pkg)

		e.SourceFiles = append(e.SourceFiles, file)
		pkg.SourceFiles = append(pkg.SourceFiles, file)
	}

	sort.SliceStable(e.SourceFiles, func(i, j int) bool {
		return strings.Compare(e.SourceFiles[i].Path, e.SourceFiles[j].Path) < 0
	})

	for _, pkg := range pkgs {
		err := e.runBuildFilesForPackage(pkg)
		if err != nil {
			return err
		}
	}

	return nil
}

func (e *Engine) runBuildFilesForPackage(pkg *packages.Package) error {
	if pkg.Globals != nil {
		return nil
	}

	re := runBuildEngine{
		Engine:         e,
		pkg:            pkg,
		registerTarget: e.defaultRegisterTarget,
	}

	err := re.runBuildFiles()
	if err != nil {
		return err
	}

	return nil
}

func (e *Engine) runBuildFileForPackage(pkg *packages.Package, file string) (starlark.StringDict, error) {
	re := runBuildEngine{
		Engine:         e,
		pkg:            pkg,
		registerTarget: e.defaultRegisterTarget,
	}

	return re.runBuildFile(file)
}

func (e *Engine) defaultRegisterTarget(spec targetspec.TargetSpec) error {
	e.TargetsLock.Lock()

	if t := e.Targets.Find(spec.FQN); t != nil {
		e.TargetsLock.Unlock()

		if !t.TargetSpec.Equal(spec) {
			return fmt.Errorf("%v is already declared and does not equal the one defined in %v\n%s\n\n%s", spec.FQN, t.Source, t.Json(), spec.Json())
		}

		return nil
	}

	e.Targets.Add(&Target{
		TargetSpec: spec,
	})
	e.TargetsLock.Unlock()

	return nil
}

type runBuildEngine struct {
	*Engine
	pkg            *packages.Package
	registerTarget func(targetspec.TargetSpec) error
	breadcrumb     []string
	targets        []targetspec.TargetSpec
	globs          []buildFileGlob
}

func (e *Engine) getOrCreatePkg(path string, fn func(fullname, name string) *packages.Package) *packages.Package {
	e.packagesMutex.Lock()
	defer e.packagesMutex.Unlock()

	if strings.HasPrefix(path, "./") || strings.HasSuffix(path, "/") {
		panic("path must be clean")
	}

	fullname := path

	if pkg, ok := e.Packages[fullname]; ok {
		return pkg
	}

	name := filepath.Base(fullname)
	if name == "." {
		name = ""
	}

	pkg := fn(fullname, name)

	if fullname != pkg.FullName {
		panic(fmt.Sprintf("pkg fullname don't match, %v %v", fullname, pkg.FullName))
	}

	e.Packages[fullname] = pkg

	return pkg
}

func (e *Engine) createPkg(path string) *packages.Package {
	return e.getOrCreatePkg(path, func(fullname, name string) *packages.Package {
		return &packages.Package{
			Name:     name,
			FullName: fullname,
			Root:     fs2.NewPath(e.Root.Abs(), path),
		}
	})
}

func (e *runBuildEngine) runBuildFiles() error {
	e.pkg.Globals = starlark.StringDict{}
	for _, file := range e.pkg.SourceFiles {
		globals, err := e.runBuildFile(file.Path)
		if err != nil {
			return err
		}

		for k, v := range globals {
			e.pkg.Globals[k] = v
		}
	}

	return nil
}

func (e *runBuildEngine) config() starlark.StringDict {
	cfg := &starlark.Dict{}

	cfg.SetKey(starlark.String("version"), starlark.String(e.Config.Version.String))

	profiles := starlark.NewList(nil)
	for _, profile := range e.Config.Profiles {
		profiles.Append(starlark.String(profile))
	}
	profiles.Freeze()
	cfg.SetKey(starlark.String("profiles"), profiles)

	for k, v := range e.Config.Extras {
		sv := utils.FromGo(v)
		sv.Freeze()
		cfg.SetKey(starlark.String(k), sv)
	}

	cfg.Freeze()

	return starlark.StringDict{
		"CONFIG": cfg,
	}
}

func (e *runBuildEngine) load(thread *starlark.Thread, module string) (starlark.StringDict, error) {
	p, err := targetspec.TargetParse(e.pkg.FullName, module)
	if err != nil {
		return nil, err
	}

	dirPkg, file := path.Split(p.Package)
	dirPkg = strings.TrimSuffix(dirPkg, "/")
	if file != "" {
		pkg, err := e.loadFromRootsOrCreatePackage(dirPkg)
		if err != nil {
			return nil, err
		}

		p := pkg.Root.Join(file).Abs()

		if fs2.PathExists(p) {
			info, _ := os.Lstat(p)
			if info.Mode().IsRegular() {
				return e.runBuildFileForPackage(pkg, p)
			}
		}
	}

	pkg, err := e.loadFromRootsOrCreatePackage(p.Package)
	if err != nil {
		return nil, err
	}

	err = e.runBuildFilesForPackage(pkg)
	if err != nil {
		return nil, fmt.Errorf("load: %w", err)
	}

	return pkg.Globals, nil
}

func (e *runBuildEngine) loadFromRootsOrCreatePackage(pkgName string) (*packages.Package, error) {
	pkg, err := e.loadFromRoots(pkgName)
	if err != nil {
		return nil, err
	}

	if pkg == nil {
		pkg = e.createPkg(pkgName)
	}

	return pkg, nil
}

func (e *runBuildEngine) buildProgram(path string, predeclared starlark.StringDict) (*starlark.Program, error) {
	info, err := os.Stat(path)
	if err != nil {
		return nil, err
	}

	h := hash.NewHash()
	h.I64(info.ModTime().Unix())
	h.UI32(uint32(info.Mode().Perm()))
	h.String(utils.Version)
	h.String(predeclaredHash)

	cachePath := e.HomeDir.Join("tmp", "__BUILD", hash.HashString(path), h.Sum()).Abs()

	err = fs2.CreateParentDir(cachePath)
	if err != nil {
		return nil, err
	}

	f, err := os.Open(cachePath)
	if err != nil && !errors.Is(err, fs.ErrNotExist) {
		return nil, err
	}

	if f != nil {
		mod, err := starlark.CompiledProgram(f)
		_ = f.Close()
		if err == nil {
			return mod, nil
		}

		log.Debugf("BUILD: compiled: %v %v", path, err)
	} else {
		log.Debugf("BUILD: no cache: %v", path)
	}

	_, mod, err := starlark.SourceProgram(path, nil, predeclared.Has)
	if err != nil {
		return nil, err
	}

	f, err = os.Create(cachePath)
	if err != nil {
		return nil, err
	}
	defer f.Close()

	err = mod.Write(f)
	if err != nil {
		log.Error(err)
	}

	return mod, err
}

func newStarlarkThread() *starlark.Thread {
	return &starlark.Thread{
		Print: func(thread *starlark.Thread, msg string) {
			log.Info(msg)
		},
	}
}

func (e *runBuildEngine) runBuildFile(path string) (starlark.StringDict, error) {
	e.mutexRunBuildFile.Get(path).Lock()
	defer e.mutexRunBuildFile.Get(path).Unlock()

	if globals, ok := e.cacheRunBuildFile.GetOk(path); ok {
		return globals, nil
	}

	globals, err := e.buildFileRunLoad(path)
	if err == nil {
		e.cacheRunBuildFile.Set(path, globals)
		return globals, nil
	}
	log.Tracef("cache %v: %v", path, err)

	log.Tracef("BUILD: running %v", path)

	thread := newStarlarkThread()
	thread.Load = e.load
	thread.SetLocal("engine", e)

	config := e.config()

	predeclaredGlobalsOnce(config)

	universe := predeclared(predeclaredGlobals, config)

	prog, err := e.buildProgram(path, universe)
	if err != nil {
		var eerr *starlark.EvalError
		if errors.As(err, &eerr) {
			return nil, fmt.Errorf("%v: %v", eerr.Msg, eerr.Backtrace())
		}
		return nil, err
	}

	globals, err = prog.Init(thread, universe)
	if err != nil {
		var eerr *starlark.EvalError
		if errors.As(err, &eerr) {
			return nil, fmt.Errorf("%v: %v", eerr.Msg, eerr.Backtrace())
		}
		return nil, err
	}

	exported := make(starlark.StringDict, len(globals))
	for k, v := range globals {
		if strings.HasPrefix(k, "_") {
			continue
		}
		exported[k] = v
	}

	e.cacheRunBuildFile.Set(path, exported)

	err = e.buildFileRunStore(path, exported)
	if err != nil {
		log.Errorf("caching %v: %v", path, err)
	}

	return exported, nil
}

func (e *runBuildEngine) buildFileRunCachePath(path string) string {
	return filepath.Join(e.HomeDir.Abs(), "tmp", "__build_cache", hash.HashString(path))
}

type buildFileCache struct {
	Hash  string
	Globs []buildFileGlob

	Targets []targetspec.TargetSpec
	Globals starlark.StringDict
}

func (e *runBuildEngine) buildFileRunHash(path string, globs []buildFileGlob) (string, error) {
	h := hash.NewHash()
	h.I64(2)
	h.String(e.Config.Version.String)
	err := e.hashFileModTimePath(h, path)
	if err != nil {
		return "", err
	}

	hash.HashMap(h, e.Config.Params, func(k string, v string) string {
		return k + v
	})

	for _, glob := range globs {
		h.String("=")
		for _, i := range glob.Include {
			h.String(i)
		}
		h.String("=")
		for _, i := range glob.Exclude {
			h.String(i)
		}
		h.String("=")
		for _, i := range glob.Result {
			h.String(i)
		}
	}

	return h.Sum(), nil
}

func (e *runBuildEngine) glob(pattern string, exclude []string) ([]string, error) {
	pkg := e.pkg

	allExclude := exclude
	allExclude = append(allExclude, "**/.heph")
	allExclude = append(allExclude, e.Config.Glob.Exclude...)

	paths := make([]string, 0)
	err := utils.StarWalk(pkg.Root.Abs(), pattern, allExclude, func(path string, d fs.DirEntry, err error) error {
		if d.IsDir() {
			return nil
		}

		paths = append(paths, path)

		return nil
	})
	if err != nil {
		return nil, err
	}

	return paths, nil
}

func (e *runBuildEngine) buildFileRunLoad(path string) (starlark.StringDict, error) {
	return nil, fmt.Errorf("a")

	f, err := os.Open(e.buildFileRunCachePath(path))
	if err != nil {
		return nil, err
	}
	defer f.Close()

	var cache buildFileCache
	err = json.NewDecoder(f).Decode(&cache)
	if err != nil {
		return nil, err
	}

	h, err := e.buildFileRunHash(path, cache.Globs)
	if err != nil {
		return nil, err
	}

	if h != cache.Hash {
		return nil, fmt.Errorf("cache mismatch current: %v cache: %v", h, cache.Hash)
	}

	log.Warnf("Cache loading %v: %v", path, len(cache.Targets))

	for _, target := range cache.Targets {
		target.Package = e.getOrCreatePkg(target.Package.FullName, func(fullname, name string) *packages.Package {
			return target.Package
		})

		err = e.registerTarget(target)
		if err != nil {
			return nil, err
		}
	}

	return cache.Globals, nil
}

func (e *runBuildEngine) buildFileRunStore(path string, globals starlark.StringDict) error {
	return nil
	for k, value := range globals {
		switch value.(type) {
		case *starlark.Function:
			log.Warnf("Cannot cache %v, %v is %v", path, k, value.Type())
			return nil
		}
	}

	h, err := e.buildFileRunHash(path, e.globs)
	if err != nil {
		return err
	}

	cache := buildFileCache{
		Hash:    h,
		Targets: e.targets,
		Globs:   e.globs,
		Globals: globals,
	}

	cachePath := e.buildFileRunCachePath(path)

	err = fs2.CreateParentDir(cachePath)
	if err != nil {
		return err
	}

	f, err := os.Create(cachePath)
	if err != nil {
		return err
	}
	defer f.Close()

	enc := json.NewEncoder(f)
	//enc.SetIndent("", "    ")
	err = enc.Encode(cache)
	if err != nil {
		return err
	}

	return nil
}
