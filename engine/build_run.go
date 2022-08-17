package engine

import (
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

func (e *Engine) runBuildFiles() error {
	walkStartTime := time.Now()
	err := utils.StarWalk(e.Root, buildFilesPattern, e.Config.BuildFiles.Ignore, func(path string, d fs.DirEntry, err error) error {
		if d.IsDir() {
			return nil
		}

		e.SourceFiles = append(e.SourceFiles, &SourceFile{
			Path: filepath.Join(e.Root, path),
		})
		return nil
	})
	if err != nil {
		return err
	}
	log.Tracef("RunBuildFiles:walk took %v", time.Now().Sub(walkStartTime))

	for _, file := range e.SourceFiles {
		pkg := e.populatePkg(file)

		pkg.SourceFiles = append(pkg.SourceFiles, file)
	}

	for _, pkg := range e.Packages {
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

func (e *Engine) createPkg(path string) *Package {
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

	pkg := &Package{
		Name:     name,
		FullName: fullname,
		Root: Path{
			RelRoot: path,
			Abs:     filepath.Join(e.Root, path),
		},
	}

	e.Packages[pkg.FullName] = pkg

	return pkg
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

func (e *runBuildEngine) runBuildFile(path string) (starlark.StringDict, error) {
	thread := &starlark.Thread{
		Load: func(thread *starlark.Thread, module string) (starlark.StringDict, error) {
			p, err := utils.TargetParse(e.pkg.FullName, module)
			if err != nil {
				return nil, err
			}

			pkg, ok := e.Packages[p.Package]
			if !ok {
				return nil, fmt.Errorf("pkg not found")
			}

			err = e.runBuildFilesForPackage(pkg)
			if err != nil {
				return nil, fmt.Errorf("load: %w", err)
			}

			return pkg.Globals, nil
		},
	}
	thread.SetLocal("engine", e)

	config := e.config()

	globals, err := predeclaredMod.Init(thread, predeclared(config))
	if err != nil {
		return nil, err
	}

	return starlark.ExecFile(thread, path, nil, predeclared(globals, config))
}
