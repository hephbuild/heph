package main

import (
	"crypto/sha256"
	"fmt"
	"io"
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

func targetName(name string, v PkgCfgVariant) string {
	return name + "@" + VID(v)
}

func libTarget(pkgs *Packages, pkg *Package) Target {
	_, imports := splitOutPkgs(pkg.Variant, pkg.Deps)

	sort.Strings(imports)

	h := sha256.New()
	for _, p := range getImportsPackages(pkg.Variant, pkgs, imports) {
		module := p.ActualModule()
		h.Write([]byte(module.Path))
		h.Write([]byte(module.Version))
	}
	suffix := fmt.Sprintf("_%.7x", h.Sum(nil))

	if pkg.IsPartOfTree {
		rel, err := filepath.Rel(Env.Root, pkg.Dir)
		if err != nil {
			panic(err)
		}
		pkgName := strings.Trim(rel, "/")

		return Target{
			Name:    targetName("_go_lib"+suffix, pkg.Variant),
			Package: normalizePackage(pkgName),
		}
	} else {
		module := pkg.ActualModule()

		importPath := pkg.ImportPath
		if pkg.Module.Replace != nil {
			importPath = strings.ReplaceAll(importPath, pkg.Module.Path, pkg.Module.Replace.Path)
		}

		return Target{
			Name:    targetName("_go_lib_"+normalizePackage(module.Version)+suffix, pkg.Variant),
			Package: filepath.Join(Config.ThirdpartyPackage, normalizePackage(importPath)),
		}
	}
}

func splitOutPkgs(variant PkgCfgVariant, pkgs []string) (stdPkgs []string, otherPkgs []string) {
	for _, p := range pkgs {
		if p == "unsafe" {
			// ignore pseudo package
			continue
		}

		if StdPackages.Get(variant).Includes(p) {
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

func getImportsPackages(variant PkgCfgVariant, pkgs *Packages, imports []string) []*Package {
	depsPkgs := make([]*Package, 0)

	for _, p := range imports {
		pkg := pkgs.Find(p, variant)
		if pkg == nil {
			fmt.Println("missing pkg for", p)
			continue
		}

		depsPkgs = append(depsPkgs, pkg)
	}

	return depsPkgs
}

func testLibFactory(name string, importLibs []string, importPath string, enabled bool, goFiles, sFiles, embedPatterns []string, libPkg string, variant PkgCfgVariant) *Lib {
	if !enabled {
		return nil
	}

	lib := &Lib{
		Target: Target{
			Name:    name,
			Package: libPkg,
		},
		ImportPath: importPath,
		GoFiles:    goFiles,
		SFiles:     sFiles,
		GenEmbed:   len(embedPatterns) > 0,
		Libs:       importLibs,
		Variant:    variant,
	}
	lib.SrcDep = srcDepForLib(lib, embedPatterns)

	return lib
}

func applyReplace(t string) string {
	if rt, ok := Config.Replace[t]; ok {
		return rt
	}

	return t
}

func srcDepForLib(lib *Lib, embedPatterns []string) []string {
	allFiles := make([]string, 0)
	allFiles = append(allFiles, lib.GoFiles...)
	allFiles = append(allFiles, lib.SFiles...)

	srcDep := make([]string, 0)
	for _, p := range allFiles {
		relRoot := filepath.Join(lib.Target.Package, p)
		if o, ok := FilesOrigin[relRoot]; ok {
			srcDep = append(srcDep, applyReplace(o))
		} else {
			srcDep = append(srcDep, p)
		}
	}

	root := filepath.Join(Env.Root, lib.Target.Package)

	for _, pattern := range embedPatterns {
		files, err := filepath.Glob(filepath.Join(root, pattern))
		if err != nil {
			panic(err)
		}

		for _, file := range files {
			relRoot, _ := filepath.Rel(Env.Root, file)
			relPkg, _ := filepath.Rel(root, file)

			if o, ok := FilesOrigin[relRoot]; ok {
				srcDep = append(srcDep, applyReplace(o))
			} else {
				srcDep = append(srcDep, relPkg)
			}
		}
	}

	return srcDep
}

func generate() []RenderUnit {
	pkgs := goListWithTransitiveTestDeps()

	units := make([]RenderUnit, 0)
	modsm := map[string]*ModDl{}

	modRoot := filepath.Dir(Env.Package)

	for _, pkg := range pkgs.Array() {
		if StdPackages.Get(pkg.Variant).Includes(pkg.ImportPath) {
			continue
		}

		fmt.Println("PKG", pkg.ImportPath, VID(pkg.Variant))

		if len(pkg.DepsErrors) > 0 {
			errs := make([]string, 0)
			for _, err := range pkg.DepsErrors {
				errs = append(errs, err.String())
			}

			panic(fmt.Sprintf("deps errors:\n%v", strings.Join(errs, "\n")))
		}

		if pkg.Error != nil {
			fmt.Println(fmt.Errorf("err: %v", pkg.Error.String()))
			continue
		}

		_, imports := splitOutPkgs(pkg.Variant, pkg.Imports)

		if pkg.IsPartOfTree {
			libPkg := libTarget(pkgs, pkg).Package
			pkgCfg := Config.GetPkgCfg(pkg.ImportPath)

			var lib *Lib
			if len(pkg.GoFiles) > 0 || len(pkg.SFiles) > 0 {
				lib = &Lib{
					Target:     libTarget(pkgs, pkg),
					ImportPath: pkg.ImportPath,
					ModRoot:    modRoot,
					GoFiles:    pkg.GoFiles,
					SFiles:     pkg.SFiles,
					GenEmbed:   len(pkg.EmbedPatterns) > 0,
					Variant:    pkg.Variant,
				}
				lib.SrcDep = srcDepForLib(lib, pkg.EmbedPatterns)

				for _, p := range imports {
					t := libTarget(pkgs, pkgs.MustFind(p, pkg.Variant))

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
					TargetName:    targetName("go_bin#build", pkg.Variant),
					TargetPackage: lib.Target.Package,
					MainLib:       lib.Target.Full(),
					Variant:       pkg.Variant,
				}

				_, deps := splitOutPkgs(pkg.Variant, pkg.Deps)

				for _, p := range deps {
					t := libTarget(pkgs, pkgs.MustFind(p, pkg.Variant))

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
				_, pkgTestDeps := splitOutPkgs(pkg.Variant, pkg.TestDeps)
				_, pkgDeps := splitOutPkgs(pkg.Variant, pkg.Deps)

				_, pkgTestImports := splitOutPkgs(pkg.Variant, pkg.TestImports)
				_, pkgImports := splitOutPkgs(pkg.Variant, pkg.Imports)

				deps := append(pkgTestDeps, pkgDeps...)
				imports := append(pkgTestImports, pkgImports...)

				importLibs := make([]string, 0)
				for _, p := range imports {
					t := libTarget(pkgs, pkgs.MustFind(p, pkg.Variant))

					importLibs = append(importLibs, t.Full())
				}

				depsLibs := make([]string, 0)
				for _, p := range deps {
					t := libTarget(pkgs, pkgs.MustFind(p, pkg.Variant))

					depsLibs = append(depsLibs, t.Full())
				}

				testlib := testLibFactory(targetName("_go_test_lib", pkg.Variant), importLibs, pkg.ImportPath, len(pkg.TestGoFiles) > 0, append(pkg.GoFiles, pkg.TestGoFiles...), pkg.SFiles, append(pkg.EmbedPatterns, pkg.TestEmbedPatterns...), libPkg, pkg.Variant)
				xtestImportLibs := importLibs
				if testlib != nil {
					xtestImportLibs = append(xtestImportLibs, testlib.Target.Full())
					depsLibs = append(depsLibs, testlib.Target.Full())
				} else if lib != nil {
					xtestImportLibs = append(xtestImportLibs, lib.Target.Full())
					depsLibs = append(depsLibs, lib.Target.Full())
				}
				xtestlib := testLibFactory(targetName("_go_xtest_lib", pkg.Variant), xtestImportLibs, pkg.ImportPath+"_test", len(pkg.XTestGoFiles) > 0, pkg.XTestGoFiles, nil, pkg.XTestEmbedPatterns, libPkg, pkg.Variant)
				if xtestlib != nil {
					depsLibs = append(depsLibs, xtestlib.Target.Full())
				}

				test := &LibTest{
					TestLib:    testlib,
					XTestLib:   xtestlib,
					ImportPath: pkg.ImportPath,
					RunExtra:   pkgCfg.Test.Run,
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
				Target:     libTarget(pkgs, pkg),
				ImportPath: pkg.ImportPath,
				ModRoot:    modRoot,
				GoFiles:    pkg.GoFiles,
				SFiles:     pkg.SFiles,
				SrcDep:     []string{moddl.Target.Full()},
				GenEmbed:   len(pkg.EmbedPatterns) > 0,
				Variant:    pkg.Variant,
			}

			for _, p := range imports {
				t := libTarget(pkgs, pkgs.MustFind(p, pkg.Variant))

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
