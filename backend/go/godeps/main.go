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
	p = strings.ReplaceAll(p, "~", "_")

	return p
}

func thirdpartyDownloadTarget(pkg *Package) Target {
	module := pkg.ActualModule()

	return Target{
		Name:    "_go_mod_download_" + normalizePackage(module.Version),
		Package: filepath.Join(Config.ThirdpartyPackage, normalizePackage(module.Path)),
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
		module := p.ActualModule()
		h.Write([]byte(module.Path))
		h.Write([]byte(module.Version))
	}
	//h.Write([]byte(Config.StdPkgsTarget)) // Allow different backends at the same time
	suffix := fmt.Sprintf("_%.7x", h.Sum(nil))

	if pkg.IsPartOfTree {
		rel, err := filepath.Rel(Env.Root, pkg.Dir)
		if err != nil {
			panic(err)
		}
		pkgName := strings.Trim(rel, "/")

		return Target{
			Name:    "_go_lib" + suffix,
			Package: normalizePackage(pkgName),
		}
	} else {
		module := pkg.ActualModule()

		importPath := pkg.ImportPath
		if pkg.Module.Replace != nil {
			importPath = strings.ReplaceAll(importPath, pkg.Module.Path, pkg.Module.Replace.Path)
		}

		return Target{
			Name:    "_go_lib_" + normalizePackage(module.Version) + suffix,
			Package: filepath.Join(Config.ThirdpartyPackage, normalizePackage(importPath)),
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
	Render func(w io.Writer)
	Dir    string
}

func getImportsPackages(pkgs *Packages, imports []string) []*Package {
	depsPkgs := make([]*Package, 0)

	for _, p := range imports {
		pkg := pkgs.Find(p)
		if pkg == nil {
			fmt.Println("missing pkg for", p)
			continue
		}

		depsPkgs = append(depsPkgs, pkg)
	}

	return depsPkgs
}

func testLibFactory(name string, importLibs []string, importPath string, enabled bool, goFiles, sFiles, embedPatterns []string, libPkg string) *Lib {
	if !enabled {
		return nil
	}

	return &Lib{
		Target: Target{
			Name:    name,
			Package: libPkg,
		},
		ImportPath:    importPath,
		GoFiles:       goFiles,
		SFiles:        sFiles,
		EmbedPatterns: embedPatterns,
		Libs:          importLibs,
	}
}

func generate() []RenderUnit {
	pkgs := goListWithTransitiveTestDeps()

	units := make([]RenderUnit, 0)
	modsm := map[string]*ModDl{}

	modRoot := filepath.Dir(Env.Package)

	for _, pkg := range pkgs.Array() {
		if len(pkg.DepsErrors) > 0 {
			errs := make([]string, 0)
			for _, err := range pkg.DepsErrors {
				errs = append(errs, err.String())
			}

			fmt.Println(fmt.Errorf("deps errors:\n%v", strings.Join(errs, "\n")))
			continue
		}

		if pkg.Error != nil {
			fmt.Println(fmt.Errorf("err: %v", pkg.Error.String()))
			continue
		}

		if StdPackages.Includes(pkg.ImportPath) {
			continue
		}

		_, imports := splitOutPkgs(pkg.Imports)

		if pkg.IsPartOfTree {
			libPkg := libTarget(pkgs, pkg, nil).Package
			pkgCfg := Config.GetPkgCfg(pkg.ImportPath)

			var lib *Lib
			if len(pkg.GoFiles) > 0 || len(pkg.SFiles) > 0 {
				lib = &Lib{
					Target:        libTarget(pkgs, pkg, nil),
					ImportPath:    pkg.ImportPath,
					ModRoot:       modRoot,
					GoFiles:       pkg.GoFiles,
					SFiles:        pkg.SFiles,
					EmbedPatterns: pkg.EmbedPatterns,
				}

				for _, p := range imports {
					t := libTarget(pkgs, pkgs.MustFind(p), nil)

					lib.Libs = append(lib.Libs, t.Full())
				}

				units = append(units, RenderUnit{
					Render: func(w io.Writer) {
						RenderLib(w, lib)
					},
					Dir: lib.Target.Package,
				})
			}

			if lib != nil && pkg.IsPartOfModule && pkg.Name == "main" {
				bin := &Bin{
					TargetPackage: lib.Target.Package,
					MainLib:       lib.Target.Full(),
				}

				_, deps := splitOutPkgs(pkg.Deps)

				for _, p := range deps {
					t := libTarget(pkgs, pkgs.MustFind(p), nil)

					bin.Libs = append(bin.Libs, t.Full())
				}

				units = append(units, RenderUnit{
					Render: func(w io.Writer) {
						RenderBin(w, bin)
					},
					Dir: bin.TargetPackage,
				})
			}

			if pkg.IsPartOfModule && !pkgCfg.Test.Skip && (len(pkg.TestGoFiles) > 0 || len(pkg.XTestGoFiles) > 0) {
				_, pkgTestDeps := splitOutPkgs(pkg.TestDeps)
				_, pkgDeps := splitOutPkgs(pkg.Deps)

				_, pkgTestImports := splitOutPkgs(pkg.TestImports)
				_, pkgImports := splitOutPkgs(pkg.Imports)

				deps := append(pkgTestDeps, pkgDeps...)
				imports := append(pkgTestImports, pkgImports...)

				importLibs := make([]string, 0)
				for _, p := range imports {
					t := libTarget(pkgs, pkgs.MustFind(p), nil)

					importLibs = append(importLibs, t.Full())
				}

				depsLibs := make([]string, 0)
				for _, p := range deps {
					t := libTarget(pkgs, pkgs.MustFind(p), nil)

					depsLibs = append(depsLibs, t.Full())
				}

				testlib := testLibFactory("_go_test_lib", importLibs, pkg.ImportPath, len(pkg.TestGoFiles) > 0, append(pkg.GoFiles, pkg.TestGoFiles...), pkg.SFiles, append(pkg.EmbedPatterns, pkg.TestEmbedPatterns...), libPkg)
				xtestImportLibs := importLibs
				if testlib != nil {
					xtestImportLibs = append(xtestImportLibs, testlib.Target.Full())
					depsLibs = append(depsLibs, testlib.Target.Full())
				} else if lib != nil {
					xtestImportLibs = append(xtestImportLibs, lib.Target.Full())
					depsLibs = append(depsLibs, lib.Target.Full())
				}
				xtestlib := testLibFactory("_go_xtest_lib", xtestImportLibs, pkg.ImportPath+"_test", len(pkg.XTestGoFiles) > 0, pkg.XTestGoFiles, nil, pkg.XTestEmbedPatterns, libPkg)
				if xtestlib != nil {
					depsLibs = append(depsLibs, xtestlib.Target.Full())
				}

				test := &LibTest{
					TestLib:    testlib,
					XTestLib:   xtestlib,
					ImportPath: pkg.ImportPath,
					PreRun:     pkgCfg.Test.PreRun,
					TestFiles:  pkg.TestGoFiles,
					XTestFiles: pkg.XTestGoFiles,
					DepsLibs:   depsLibs,
				}

				units = append(units, RenderUnit{
					Render: func(w io.Writer) {
						RenderTest(w, test)
					},
					Dir: libPkg,
				})
			}
		} else {
			if pkg.Module == nil {
				fmt.Printf("missing module for %v\n", pkg.ImportPath)
				continue
			}

			module := pkg.ActualModule()

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
					Dir: moddl.Target.Package,
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
				GenEmbed:   len(pkg.EmbedPatterns) > 0,
			}

			for _, p := range imports {
				t := libTarget(pkgs, pkgs.MustFind(p), nil)

				lib.Libs = append(lib.Libs, t.Full())
			}

			units = append(units, RenderUnit{
				Render: func(w io.Writer) {
					RenderLib(w, lib)
				},
				Dir: lib.Target.Package,
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
	case "embed":
		genEmbed()
	default:
		panic("unhandled mode " + os.Args[1])
	}
}
