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

const buildFilesPattern = "**/{BUILD,BUILD.heph}"

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

	e.Packages = map[string]*Package{}
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

func (e *runBuildEngine) predeclared() starlark.StringDict {
	predeclared := starlark.StringDict{}
	predeclared["target"] = starlark.NewBuiltin("target", e.target)
	predeclared["glob"] = starlark.NewBuiltin("glob", e.glob)
	predeclared["package_name"] = starlark.NewBuiltin("package_name", e.package_name)
	predeclared["package_fqn"] = starlark.NewBuiltin("package_fqn", e.package_fqn)
	predeclared["get_os"] = starlark.NewBuiltin("get_os", e.get_os)
	predeclared["get_arch"] = starlark.NewBuiltin("get_arch", e.get_arch)
	predeclared["set_deps"] = starlark.NewBuiltin("set_deps", e.set_deps)
	predeclared["to_json"] = starlark.NewBuiltin("to_json", e.to_json)

	return predeclared
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
	thread.SetLocal("pkg", e.pkg)

	return starlark.ExecFile(thread, path, nil, e.predeclared())
}
