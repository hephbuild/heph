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

func (e *Engine) runBuildFiles() error {
	walkStartTime := time.Now()
	err := utils.StarWalk(e.Root, "**/{BUILD,BUILD.heph}", e.Config.BuildFiles.Ignore, func(path string, d fs.DirEntry, err error) error {
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
	}

	err := re.runBuildFiles()
	if err != nil {
		return err
	}

	return nil
}

type runBuildEngine struct {
	*Engine
	pkg *Package
}

func (e *Engine) populatePkg(file *SourceFile) *Package {
	root := filepath.Dir(file.Path)

	relRoot, err := filepath.Rel(e.Root, root)
	if err != nil {
		panic(err)
	}
	if relRoot == "." { // filepath.Rel may return .
		relRoot = ""
	}

	fullname := strings.Trim(relRoot, "/")

	if pkg, ok := e.Packages[fullname]; ok {
		return pkg
	}

	pkg := &Package{
		Name:     filepath.Base(root),
		FullName: fullname,
		Root: Path{
			RelRoot: relRoot,
			Abs:     root,
		},
		Thread: &starlark.Thread{
			Name: fullname,
			Load: func(thread *starlark.Thread, module string) (starlark.StringDict, error) {
				p, err := utils.TargetParse(fullname, module)
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
		},
	}

	pkg.Thread.SetLocal("pkg", pkg)

	e.Packages[pkg.FullName] = pkg

	return pkg
}

func (e *runBuildEngine) runBuildFiles() error {
	predeclared := starlark.StringDict{}
	predeclared["target"] = starlark.NewBuiltin("target", e.addTarget)
	predeclared["glob"] = starlark.NewBuiltin("glob", e.glob)
	predeclared["package_name"] = starlark.NewBuiltin("package_name", e.package_name)
	predeclared["get_os"] = starlark.NewBuiltin("get_os", e.get_os)
	predeclared["get_arch"] = starlark.NewBuiltin("get_arch", e.get_arch)

	e.pkg.Globals = starlark.StringDict{}
	for _, file := range e.pkg.SourceFiles {
		globals, err := starlark.ExecFile(e.pkg.Thread, file.Path, nil, predeclared)
		if err != nil {
			return err
		}

		for k, v := range globals {
			e.pkg.Globals[k] = v
		}
	}

	return nil
}
