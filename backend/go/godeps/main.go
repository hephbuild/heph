package main

import (
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
)

// Build pipeline from:
// https://github.com/golang/go/blob/2c46cc8b8997f4f5cdb7766e4e2bdf8e57f67c76/src/cmd/go/internal/work/exec.go

func normalizePackage(p string) string {
	p = strings.ReplaceAll(p, "+", "_")

	return p
}

func thirdpartyDownloadTarget(pkg *Package) Target {
	return Target{
		Name:    "_go_mod_download_" + normalizePackage(pkg.Module.Version),
		Package: filepath.Join(Config.ThirdpartyPackage, pkg.Module.Path),
	}
}

func libTarget(pkg *Package) Target {
	if pkg.IsPartOfTree {
		rel, err := filepath.Rel(Env.Sandbox, pkg.Dir)
		if err != nil {
			panic(err)
		}
		pkgName := strings.Trim(rel, "/")

		return Target{
			Name:    "_go_lib",
			Package: pkgName,
		}
	} else {
		return Target{
			Name:    "_go_lib_" + normalizePackage(pkg.Module.Version),
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

	for _, pkg := range pkgs {
		if StdPackages.Includes(pkg.ImportPath) {
			continue
		}

		_, imports := splitOutPkgs(pkg.Imports)

		if pkg.IsPartOfTree {
			lib := &Lib{
				Target:     libTarget(pkg),
				ImportPath: pkg.ImportPath,
				ModRoot:    modRoot,
				GoFiles:    pkg.GoFiles,
				SFiles:     pkg.SFiles,
			}

			for _, p := range imports {
				t := libTarget(pkgs.Find(p))

				lib.Libs = append(lib.Libs, t.Full())
			}

			units = append(units, RenderUnit{
				Render: func(w io.Writer) {
					RenderLib(w, lib)
				},
				Package: lib.Target.Package,
			})

			if pkg.IsPartOfModule && !Config.IsTestSkipped(pkg.ImportPath) && (len(pkg.TestGoFiles) > 0 || len(pkg.XTestGoFiles) > 0) {
				_, pkgTestDeps := splitOutPkgs(pkg.TestDeps)
				_, pkgDeps := splitOutPkgs(pkg.Deps)

				imports := append(pkgTestDeps, pkgDeps...)
				imports = append(imports, pkg.ImportPath)

				goFiles := make([]string, 0)
				goFiles = append(goFiles, lib.GoFiles...)
				goFiles = append(goFiles, pkg.TestGoFiles...)
				goFiles = append(goFiles, pkg.XTestGoFiles...)

				test := &LibTest{
					ImportPath:    pkg.ImportPath,
					TargetPackage: lib.Target.Package,
					GoFiles:       goFiles,
					SFiles:        lib.SFiles,
					PreRun:        Config.Test.PreRun,
					TestFiles:     pkg.TestGoFiles,
					XTestFiles:    pkg.XTestGoFiles,
				}

				for _, p := range imports {
					t := libTarget(pkgs.Find(p))

					test.Libs = append(test.Libs, t.Full())
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

			id := pkg.Module.Path + pkg.Module.Version

			moddl, exists := modsm[id]
			if !exists {
				moddl = &ModDl{
					Target:  thirdpartyDownloadTarget(pkg),
					Path:    pkg.Module.Path,
					Version: pkg.Module.Version,
				}

				if mod := pkg.Module.Replace; mod != nil {
					moddl.Path = mod.Path
					moddl.Version = mod.Version
				}

				units = append(units, RenderUnit{
					Render: func(w io.Writer) {
						RenderModDl(w, moddl)
					},
					Package: moddl.Target.Package,
				})

				modsm[id] = moddl
			}

			lib := &Lib{
				Target:     libTarget(pkg),
				ImportPath: pkg.ImportPath,
				ModRoot:    modRoot,
				GoFiles:    pkg.GoFiles,
				SFiles:     pkg.SFiles,
				SrcDep:     moddl.Target.Full(),
			}

			for _, p := range imports {
				t := libTarget(pkgs.Find(p))

				lib.Libs = append(lib.Libs, t.Full())
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
