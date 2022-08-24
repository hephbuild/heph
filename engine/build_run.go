package engine

import (
	"errors"
	"fmt"
	log "github.com/sirupsen/logrus"
	"go.starlark.net/resolve"
	"go.starlark.net/starlark"
	"heph/utils"
	"io/fs"
	"path/filepath"
	"strings"
	"time"
)

func init() {
	resolve.AllowGlobalReassign = true
}

const buildFilesPattern = "**/{BUILD,BUILD.*}"

func (e *Engine) collectBuildFiles(root string) (SourceFiles, error) {
	files := make(SourceFiles, 0)

	walkStartTime := time.Now()
	err := utils.StarWalk(root, buildFilesPattern, e.Config.BuildFiles.Ignore, func(path string, d fs.DirEntry, err error) error {
		if d.IsDir() {
			return nil
		}

		files = append(files, &SourceFile{
			Path: filepath.Join(root, path),
		})
		return nil
	})
	if err != nil {
		return nil, err
	}
	log.Tracef("RunBuildFiles:walk took %v", time.Now().Sub(walkStartTime))

	return files, err
}

func (e *Engine) runBuildFiles(root string, pkgProvider func(dir string) *Package) error {
	files, err := e.collectBuildFiles(root)
	if err != nil {
		return err
	}

	pkgs := make([]*Package, 0)
	for _, file := range files {
		relRoot, err := filepath.Rel(e.Root, filepath.Dir(file.Path))
		if err != nil {
			panic(err)
		}

		pkg := pkgProvider(relRoot)

		pkgs = append(pkgs, pkg)

		e.SourceFiles = append(e.SourceFiles, file)
		pkg.SourceFiles = append(pkg.SourceFiles, file)
	}

	for _, pkg := range pkgs {
		err := e.runBuildFilesForPackage(pkg)
		if err != nil {
			return err
		}
	}

	return nil
}

func (e *Engine) runBuildFilesForPackage(pkg *Package) error {
	if pkg.Globals != nil {
		return nil
	}

	re := runBuildEngine{
		Engine: e,
		pkg:    pkg,
		registerTarget: func(t TargetSpec) error {
			e.TargetsLock.Lock()
			defer e.TargetsLock.Unlock()

			e.Targets = append(e.Targets, &Target{
				TargetSpec: t,
			})

			return nil
		},
	}

	err := re.runBuildFiles()
	if err != nil {
		return err
	}

	return nil
}

type runBuildEngine struct {
	*Engine
	pkg            *Package
	registerTarget func(TargetSpec) error
}

func (e *Engine) getOrCreatePkg(path string, fn func(fullname, name string) *Package) *Package {
	e.packagesMutex.Lock()
	defer e.packagesMutex.Unlock()

	fullname := strings.Trim(path, "/.")

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

func (e *Engine) createPkg(path string) *Package {
	return e.getOrCreatePkg(path, func(fullname, name string) *Package {
		return &Package{
			Name:     name,
			FullName: fullname,
			Root: Path{
				RelRoot: path,
				Abs:     filepath.Join(e.Root, path),
			},
		}
	})
}

func (e *Engine) populatePkg(file *SourceFile) *Package {
	root := filepath.Dir(file.Path)

	relRoot, err := filepath.Rel(e.Root, root)
	if err != nil {
		panic(err)
	}

	return e.createPkg(relRoot)
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
	for k, v := range e.Config.Extras {
		cfg.SetKey(starlark.String(k), utils.FromGo(v))
	}

	return starlark.StringDict{
		"CONFIG": cfg,
	}
}

func (e *runBuildEngine) load(thread *starlark.Thread, module string) (starlark.StringDict, error) {
	p, err := utils.TargetParse(e.pkg.FullName, module)
	if err != nil {
		return nil, err
	}

	pkg, err := e.loadFromRoots(p.Package)
	if err != nil {
		return nil, err
	}

	if pkg == nil {
		pkg = e.createPkg(p.Package)
	}

	err = e.runBuildFilesForPackage(pkg)
	if err != nil {
		return nil, fmt.Errorf("load: %w", err)
	}

	return pkg.Globals, nil
}

func (e *runBuildEngine) runBuildFile(path string) (starlark.StringDict, error) {
	log.Tracef("BUILD: running %v", path)

	thread := &starlark.Thread{
		Load: e.load,
	}
	thread.SetLocal("engine", e)

	config := e.config()

	globals, err := predeclaredMod.Init(thread, predeclared(config))
	if err != nil {
		return nil, err
	}
	globals.Freeze()

	res, err := starlark.ExecFile(thread, path, nil, predeclared(globals, config))
	if err != nil {
		var eerr *starlark.EvalError
		if errors.As(err, &eerr) {
			return nil, fmt.Errorf("%v: %v", eerr.Msg, eerr.Backtrace())
		}
		return nil, err
	}

	return res, nil
}
