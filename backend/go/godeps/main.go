package main

import (
	"crypto/sha256"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

// Build pipeline from:
// https://github.com/golang/go/blob/2c46cc8b8997f4f5cdb7766e4e2bdf8e57f67c76/src/cmd/go/internal/work/exec.go

func normalizePackage(p string) string {
	p = strings.ReplaceAll(p, "+", "_")

	return p
}

func thirdpartyDownloadTarget(pkg *Package) Target {
	module := pkg.Module.Actual()

	return Target{
		Name:    "_go_mod_download_" + normalizePackage(module.Version),
		Package: filepath.Join(Config.ThirdpartyPackage, module.Path),
	}
}

func libTarget(pkgs *Packages, pkg *Package, imports []string) Target {
	if imports == nil {
		imports = pkg.Deps
	}

	_, imports = splitOutPkgs(imports)

	sort.Strings(imports)

	h := sha256.New()
	for _, p := range getImportsPackages(pkgs, imports) {
		module := p.Module.Actual()
		h.Write([]byte(module.Path))
		h.Write([]byte(module.Version))
	}
	suffix := fmt.Sprintf("_%.7x", h.Sum(nil))

	if pkg.IsPartOfTree {
		rel, err := filepath.Rel(Env.Sandbox, pkg.Dir)
		if err != nil {
			panic(err)
		}
		pkgName := strings.Trim(rel, "/")

		return Target{
			Name:    "_go_lib" + suffix,
			Package: pkgName,
		}
	} else {
		module := pkg.Module.Actual()

		importPath := pkg.ImportPath
		if pkg.Module.Replace != nil {
			importPath = strings.ReplaceAll(importPath, pkg.Module.Path, pkg.Module.Replace.Path)
		}

		return Target{
			Name:    "_go_lib_" + normalizePackage(module.Version) + suffix,
			Package: filepath.Join(Config.ThirdpartyPackage, importPath),
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

func getImportsPackages(pkgs *Packages, imports []string) []*Package {
	depsPkgs := make([]*Package, 0)

	for _, p := range imports {
		pkg := pkgs.Find(p)

		depsPkgs = append(depsPkgs, pkg)
	}

	return depsPkgs
}

func generate() []RenderUnit {
	pkgs := goListWithTransitiveTestDeps()

	units := make([]RenderUnit, 0)
	modsm := map[string]*ModDl{}

	modRoot := filepath.Dir(Env.Package)

	for _, pkg := range pkgs.Array() {
		if StdPackages.Includes(pkg.ImportPath) {
			continue
		}

		_, imports := splitOutPkgs(pkg.Imports)

		if pkg.IsPartOfTree {
			lib := &Lib{
				Target:     libTarget(pkgs, pkg, nil),
				ImportPath: pkg.ImportPath,
				ModRoot:    modRoot,
				GoFiles:    pkg.GoFiles,
				SFiles:     pkg.SFiles,
			}

			for _, p := range imports {
				t := libTarget(pkgs, pkgs.Find(p), nil)

				lib.Libs = append(lib.Libs, t.Full())
			}

			units = append(units, RenderUnit{
				Render: func(w io.Writer) {
					RenderLib(w, lib)
				},
				Package: lib.Target.Package,
			})

			if pkg.IsPartOfModule && pkg.Name == "main" {
				bin := &Bin{
					TargetPackage: lib.Target.Package,
					MainLib:       lib.Target.Full(),
				}

				_, deps := splitOutPkgs(pkg.Deps)

				for _, p := range deps {
					t := libTarget(pkgs, pkgs.Find(p), nil)

					bin.Libs = append(bin.Libs, t.Full())
				}

				units = append(units, RenderUnit{
					Render: func(w io.Writer) {
						RenderBin(w, bin)
					},
					Package: bin.TargetPackage,
				})
			}

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
					t := libTarget(pkgs, pkgs.Find(p), nil)

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

			module := pkg.Module.Actual()

			target := thirdpartyDownloadTarget(pkg)

			moddl, exists := modsm[target.Full()]
			if !exists {
				moddl = &ModDl{
					Target:  target,
					Path:    module.Path,
					Version: module.Version,
				}

				units = append(units, RenderUnit{
					Render: func(w io.Writer) {
						RenderModDl(w, moddl)
					},
					Package: moddl.Target.Package,
				})

				modsm[target.Full()] = moddl
			}

			lib := &Lib{
				Target:     libTarget(pkgs, pkg, nil),
				ImportPath: pkg.ImportPath,
				ModRoot:    modRoot,
				GoFiles:    pkg.GoFiles,
				SFiles:     pkg.SFiles,
				SrcDep:     moddl.Target.Full(),
			}

			for _, p := range imports {
				t := libTarget(pkgs, pkgs.Find(p), nil)

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
	switch os.Args[1] {
	case "mod":
		genBuild()
	case "imports":
		listImports()
	default:
		panic("unhandled mode")
	}
}
