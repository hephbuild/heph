package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"gobackend/log"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
)

type Mod struct {
	Main    bool
	Path    string
	Dir     string
	Version string
	Replace *Mod
}

type Error struct {
	ImportStack []string
	Pos         string
	Err         string
}

func (e Error) String() string {
	return fmt.Sprintf("%v %v %v", e.Err, e.Pos, strings.Join(e.ImportStack, " "))
}

// https://pkg.go.dev/cmd/go#hdr-List_packages_or_modules

type Package struct {
	Dir         string
	Root        string
	Name        string
	ImportPath  string
	Module      *Mod
	Standard    bool
	Deps        []string
	Imports     []string
	TestImports []string
	GoFiles     []string
	//IgnoredGoFiles []string
	TestGoFiles  []string
	XTestGoFiles []string
	CFiles       []string
	CXXFiles     []string
	SFiles       []string

	EmbedPatterns      []string
	TestEmbedPatterns  []string
	XTestEmbedPatterns []string

	// Part of our own module
	IsPartOfModule bool          `json:"-"`
	IsPartOfTree   bool          `json:"-"`
	TestDeps       []string      `json:"-"`
	Variant        PkgCfgVariant `json:"-"`

	DepsErrors []Error
	Error      *Error
}

func (p *Package) ActualModule() *Mod {
	if p.Module == nil {
		fmt.Println(p.ImportPath, "does not have a module, try running go mod tidy ?")
		os.Exit(1)
	}

	if p.Module.Replace != nil {
		return p.Module.Replace
	}

	return p.Module
}

type Packages struct {
	m map[string]*Package
	a []*Package
}

func (p *Packages) Array() []*Package {
	return p.a
}

func (p *Packages) Sort() {
	sort.SliceStable(p.a, func(i, j int) bool {
		c := strings.Compare(p.a[i].ImportPath, p.a[j].ImportPath)
		if c == 0 {
			vstri := VID(p.a[i].Variant)
			vstrj := VID(p.a[j].Variant)

			return strings.Compare(vstri, vstrj) < 0
		}

		return c < 0
	})
}

func (p *Packages) Add(pkg *Package) {
	if p.m == nil {
		p.m = map[string]*Package{}
	}
	p.m[p.key(pkg.ImportPath, VID(pkg.Variant))] = pkg
	p.a = append(p.a, pkg)
}

func (p *Packages) MustFind(importPath string, variant PkgCfgVariant) *Package {
	pkg := p.Find(importPath, variant)
	if pkg == nil {
		panic(fmt.Sprintf("unable to find package for module %v with %v", importPath, VID(variant)))
	}

	return pkg
}

func (p *Packages) key(importPath, vstr string) string {
	return importPath + "_" + vstr
}

func VID(variant PkgCfgVariant) string {
	s := fmt.Sprintf("os=%v,arch=%v", variant.OS, variant.ARCH)

	if len(variant.Tags) > 0 {
		s += fmt.Sprintf(",tags={%v}", strings.Join(variant.Tags, ","))
	}

	return s
}

func (p *Packages) Find(importPath string, variant PkgCfgVariant) *Package {
	return p.m[p.key(importPath, VID(variant))]
}

type Strings []string

func (ss Strings) Includes(s string) bool {
	for _, sc := range ss {
		if sc == s {
			return true
		}
	}

	return false
}

type stdPackages struct {
	m map[string]Strings
}

func (sp *stdPackages) Get(variant PkgCfgVariant) Strings {
	k := VID(variant)
	if s, ok := sp.m[k]; ok {
		return s
	}

	sp.m[k] = goListStd(variant)

	return sp.m[k]
}

var StdPackages = stdPackages{map[string]Strings{}}

func goListStd(variant PkgCfgVariant) Strings {
	log.Debug("go list std")
	args := []string{"list"}
	if len(variant.Tags) > 0 {
		args = append(args, "-tags", strings.Join(variant.Tags, ","))
	}
	args = append(args, "std")

	cmd := exec.Command("go", args...)
	cmd.Env = append(os.Environ(), []string{
		"GOOS=" + variant.OS,
		"GOARCH=" + variant.ARCH,
	}...)

	b, err := cmd.Output()
	if err != nil {
		var eerr *exec.ExitError
		if errors.As(err, &eerr) {
			panic(string(eerr.Stderr))
		}
		panic(err)
	}

	s := string(b)
	s = strings.TrimSpace(s)

	log.Debug(s)

	return strings.Split(s, "\n")
}

func goEnv(name string) string {
	cmd := exec.Command("go", "env", name)
	b, err := cmd.Output()
	if err != nil {
		panic(err)
	}

	return strings.TrimSpace(string(b))
}

func goListImportPaths(pkg string) []string {
	cmd := exec.Command("go", "list", "-e", pkg)

	b, err := cmd.Output()
	if err != nil {
		var eerr *exec.ExitError
		if errors.As(err, &eerr) {
			panic(string(eerr.Stderr))
		}
		panic(err)
	}

	return strings.Split(string(b), "\n")
}

func goList(pkg string, variant PkgCfgVariant) []*Package {
	args := []string{"list", "-e", "-json", "-deps"}
	if len(variant.Tags) > 0 {
		args = append(args, "-tags", strings.Join(variant.Tags, ","))
	}
	args = append(args, pkg)
	cmd := exec.Command("go", args...)
	cmd.Env = append(os.Environ(), []string{
		"GOOS=" + variant.OS,
		"GOARCH=" + variant.ARCH,
	}...)

	b, err := cmd.Output()
	if err != nil {
		var eerr *exec.ExitError
		if errors.As(err, &eerr) {
			panic(string(eerr.Stderr))
		}
		panic(err)
	}

	pkgs := make([]*Package, 0)

	if log.Enabled() {
		cwd, _ := os.Getwd()
		log.Debug("### go list ", pkg, cwd)
		log.Debug(string(b))
	}

	dec := json.NewDecoder(bytes.NewReader(b))
	for {
		var pkg Package

		err := dec.Decode(&pkg)
		if err == io.EOF {
			// all done
			break
		}
		if err != nil {
			panic(err)
		}

		pkg.Variant = variant

		pkgs = append(pkgs, &pkg)
	}

	return pkgs
}

func isPathUnder(path, under string) bool {
	rel, _ := filepath.Rel(under, path)
	return len(rel) > 0 && !strings.Contains(rel, "..")
}

func analyzePkg(pkg *Package) {
	if pkg.Module != nil {
		partOfTree := isPathUnder(pkg.Module.Dir, Env.Root)
		thirdparty := isPathUnder(pkg.Module.Dir, Env.GOPATH)

		pkg.IsPartOfTree = partOfTree && !thirdparty
	}

	if pkg.IsPartOfTree {
		pkg.IsPartOfModule = isPathUnder(pkg.Dir, filepath.Join(Env.Root, Env.Package))
	}

	if !StdPackages.Get(pkg.Variant).Includes(pkg.ImportPath) {
		log.Debugln(pkg.Dir)
		log.Debugf("  IsPartOfTree: %v IsPartOfModule: %v\n", pkg.IsPartOfTree, pkg.IsPartOfModule)
	}
}

func goListWithTransitiveTestDeps(pkg string) *Packages {
	allPkgs := &Packages{}

	for _, impPath := range goListImportPaths(pkg) {
		cfg := Config.GetPkgCfg(impPath)

		variants := cfg.Variants
		if len(variants) == 0 {
			variants = append(variants, PkgCfgVariant{
				OS:   Env.GOOS,
				ARCH: Env.GOARCH,
			})
		}

		for _, variant := range variants {
			log.Debugf("VARIANT %v", VID(variant))
			pkgs := goList(impPath, variant)

			for _, pkg := range pkgs {
				if allPkgs.Find(pkg.ImportPath, pkg.Variant) != nil {
					continue
				}

				analyzePkg(pkg)
				allPkgs.Add(pkg)

				if pkg.IsPartOfModule {
					// We only care about transitive test deps of the stuff we will test

					testDeps := make([]string, 0)

					for _, depPath := range pkg.TestImports {
						testDeps = append(testDeps, depPath)
						for _, p := range goList(depPath, pkg.Variant) {
							testDeps = append(testDeps, p.ImportPath)

							if allPkgs.Find(p.ImportPath, p.Variant) == nil {
								analyzePkg(p)
								allPkgs.Add(p)
							}
						}
					}

					pkg.TestDeps = testDeps
				}
			}
		}
	}

	allPkgs.Sort()

	return allPkgs
}
