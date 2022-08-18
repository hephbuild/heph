package main

import (
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
)

func thirdpartyDownloadTarget(pkg *Package) Target {
	return Target{
		Name:    "_go_mod_download",
		Package: filepath.Join(Config.ThirdpartyPackage, pkg.Module.Path),
	}
}

func libTarget(pkg *Package) Target {
	if pkg.IsPartOfModule {
		pkgName := filepath.Join(Env.Package, strings.TrimPrefix(pkg.ImportPath, filepath.Base(Env.Package)))
		pkgName = strings.TrimPrefix(pkgName, "/")

		return Target{
			Name:    "_go_lib",
			Package: pkgName,
		}
	} else {
		return Target{
			Name:    "_go_lib",
			Package: filepath.Join(Config.ThirdpartyPackage, pkg.ImportPath),
		}
	}
}

func splitOutPkgs(pkgs []string) (stdPkgs []string, otherPkgs []string) {
	for _, p := range pkgs {
		if p == "unsafe" {
			// ignore pseudo package
			continue
		}

		if StdPackages.Includes(p) {
			stdPkgs = append(stdPkgs, p)
			continue
		}

		otherPkgs = append(otherPkgs, p)
	}

	return stdPkgs, otherPkgs
}

type RenderUnit struct {
	Render  func(w io.Writer)
	Package string
}

func generate() []RenderUnit {
	pkgs := goListWithTransitiveTestDeps()

	units := make([]RenderUnit, 0)
	modsm := map[string]*ModDl{}

	modRoot := filepath.Dir(Env.Package)

	stdPkgs := goListStd()

	for _, pkg := range pkgs {
		if stdPkgs.Includes(pkg.ImportPath) {
			continue
		}

		_, imports := splitOutPkgs(pkg.Imports)

		if pkg.IsPartOfModule {
			deps := make([]string, 0)
			deps = append(deps, pkg.GoFiles...)

			for _, p := range imports {
				t := libTarget(pkgs.Find(p))

				deps = append(deps, t.Full())
			}

			lib := &Lib{
				Target:       libTarget(pkg),
				ImportPath:   pkg.ImportPath,
				ModRoot:      modRoot,
				Deps:         deps,
				CompileFiles: []string{"*.go"},
			}

			units = append(units, RenderUnit{
				Render: func(w io.Writer) {
					RenderLib(w, lib)
				},
				Package: lib.Target.Package,
			})

			if len(pkg.TestGoFiles) > 0 || len(pkg.XTestGoFiles) > 0 {
				_, pkgTestDeps := splitOutPkgs(pkg.TestDeps)
				_, pkgDeps := splitOutPkgs(pkg.Imports)

				// tests depend on self lib
				imports = append(imports, pkg.ImportPath)

				deps := make([]string, 0)

				deps = append(deps, pkg.GoFiles...)
				deps = append(deps, pkg.TestGoFiles...)
				deps = append(deps, pkg.XTestGoFiles...)

				for _, p := range append(pkgTestDeps, pkgDeps...) {
					t := libTarget(pkgs.Find(p))

					deps = append(deps, t.Full())
				}

				test := &LibTest{
					ImportPath:    pkg.ImportPath,
					TargetPackage: lib.Target.Package,
					Deps:          deps,
					PreRun:        Config.Test.PreRun,
					TestFiles:     pkg.TestGoFiles,
					XTestFiles:    pkg.XTestGoFiles,
				}

				units = append(units, RenderUnit{
					Render: func(w io.Writer) {
						RenderTest(w, test)
					},
					Package: test.TargetPackage,
				})
			}
		} else {
			if pkg.Module == nil {
				fmt.Printf("missing module for %v\n", pkg.ImportPath)
				continue
			}

			moddl, exists := modsm[pkg.Module.Path]
			if !exists {
				moddl = &ModDl{
					Target:  thirdpartyDownloadTarget(pkg),
					Path:    pkg.Module.Path,
					Version: pkg.Module.Version,
				}

				units = append(units, RenderUnit{
					Render: func(w io.Writer) {
						RenderModDl(w, moddl)
					},
					Package: moddl.Target.Package,
				})

				modsm[pkg.Module.Path] = moddl
			}

			deps := make([]string, 0)
			deps = append(deps, moddl.Target.Full())
			for _, p := range imports {
				t := libTarget(pkgs.Find(p))

				deps = append(deps, t.Full())
			}

			lib := &Lib{
				Target:       libTarget(pkg),
				ImportPath:   pkg.ImportPath,
				ModRoot:      modRoot,
				Deps:         deps,
				CompileFiles: pkg.GoFiles,
			}

			units = append(units, RenderUnit{
				Render: func(w io.Writer) {
					RenderLib(w, lib)
				},
				Package: lib.Target.Package,
			})
		}
	}

	return units
}

func main() {
	unitsPerPackage := map[string][]RenderUnit{}

	for _, unit := range generate() {
		unitsPerPackage[unit.Package] = append(unitsPerPackage[unit.Package], unit)
	}

	for pkg, units := range unitsPerPackage {
		err := os.MkdirAll(filepath.Join(Env.Sandbox, pkg), os.ModePerm)
		if err != nil {
			panic(err)
		}

		f, err := os.Create(filepath.Join(Env.Sandbox, pkg, "BUILD"))
		if err != nil {
			panic(err)
		}

		for _, unit := range units {
			unit.Render(f)
		}

		f.Close()
	}
}
