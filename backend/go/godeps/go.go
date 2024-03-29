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
	Dir            string   // directory containing package sources
	ImportPath     string   // import path of package in dir
	ImportComment  string   // path in import comment on package statement
	Name           string   // package name
	Doc            string   // package documentation string
	Target         string   // install path
	Shlib          string   // the shared library that contains this package (only set when -linkshared)
	Goroot         bool     // is this package in the Go root?
	Standard       bool     // is this package part of the standard Go library?
	Stale          bool     // would 'go install' do anything for this package?
	StaleReason    string   // explanation for Stale==true
	Root           string   // Go root or Go path dir containing this package
	ConflictDir    string   // this directory shadows Dir in $GOPATH
	BinaryOnly     bool     // binary-only package (no longer supported)
	ForTest        string   // package is only for use in named test
	Export         string   // file containing export data (when using -export)
	BuildID        string   // build ID of the compiled package (when using -export)
	Module         *Mod     // info about package's containing module, if any (can be nil)
	Match          []string // command-line patterns matching this package
	DepOnly        bool     // package is only a dependency, not explicitly listed
	DefaultGODEBUG string   // default GODEBUG setting, for main packages

	// Source files
	GoFiles           []string // .go source files (excluding CgoFiles, TestGoFiles, XTestGoFiles)
	CgoFiles          []string // .go source files that import "C"
	CompiledGoFiles   []string // .go files presented to compiler (when using -compiled)
	IgnoredGoFiles    []string // .go source files ignored due to build constraints
	IgnoredOtherFiles []string // non-.go source files ignored due to build constraints
	CFiles            []string // .c source files
	CXXFiles          []string // .cc, .cxx and .cpp source files
	MFiles            []string // .m source files
	HFiles            []string // .h, .hh, .hpp and .hxx source files
	FFiles            []string // .f, .F, .for and .f90 Fortran source files
	SFiles            []string // .s source files
	SwigFiles         []string // .swig files
	SwigCXXFiles      []string // .swigcxx files
	SysoFiles         []string // .syso object files to add to archive
	TestGoFiles       []string // _test.go files in package
	XTestGoFiles      []string // _test.go files outside package

	// Embedded files
	EmbedPatterns      []string // //go:embed patterns
	EmbedFiles         []string // files matched by EmbedPatterns
	TestEmbedPatterns  []string // //go:embed patterns in TestGoFiles
	TestEmbedFiles     []string // files matched by TestEmbedPatterns
	XTestEmbedPatterns []string // //go:embed patterns in XTestGoFiles
	XTestEmbedFiles    []string // files matched by XTestEmbedPatterns

	// Cgo directives
	CgoCFLAGS    []string // cgo: flags for C compiler
	CgoCPPFLAGS  []string // cgo: flags for C preprocessor
	CgoCXXFLAGS  []string // cgo: flags for C++ compiler
	CgoFFLAGS    []string // cgo: flags for Fortran compiler
	CgoLDFLAGS   []string // cgo: flags for linker
	CgoPkgConfig []string // cgo: pkg-config names

	// Dependency information
	Imports      []string          // import paths used by this package
	ImportMap    map[string]string // map from source import to ImportPath (identity entries omitted)
	Deps         []string          // all (recursively) imported dependencies
	TestImports  []string          // imports from TestGoFiles
	XTestImports []string          // imports from XTestGoFiles

	// Error information
	Incomplete bool     // this package or a dependency has an error
	Error      *Error   // error loading package
	DepsErrors []*Error // errors loading dependencies

	// Part of our own module
	IsPartOfModule bool          `json:"-"`
	IsPartOfTree   bool          `json:"-"`
	TestDeps       []string      `json:"-"`
	XTestDeps      []string      `json:"-"`
	Variant        PkgCfgVariant `json:"-"`
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
	if variant.Name != "" {
		return variant.Name
	}

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
	cmd := exec.Command("go", "list", "-e", "-find", "-a", pkg)

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
	return goListm([]string{pkg}, variant)
}

func goListm(pkg []string, variant PkgCfgVariant) []*Package {
	args := []string{"list", "-e", "-json", "-deps"}
	if len(variant.Tags) > 0 {
		args = append(args, "-tags", strings.Join(variant.Tags, ","))
	}
	args = append(args, pkg...)
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

func goListWithTransitiveTestDeps() *Packages {
	type variantPkgs struct {
		variant PkgCfgVariant
		pkgs    []string
	}

	pkgsPerVariant := map[string]variantPkgs{}

	for _, pkg := range goListImportPaths("./...") {
		cfg := Config.GetPkgCfg(pkg)

		for _, variant := range cfg.VariantsDefault() {
			vid := VID(variant)

			pv := pkgsPerVariant[vid]
			pv.variant = variant
			pv.pkgs = append(pv.pkgs, pkg)
			pkgsPerVariant[vid] = pv
		}
	}

	allPkgs := &Packages{}

	for _, pv := range pkgsPerVariant {
		pkgs := goListm(pv.pkgs, pv.variant)

		for _, pkg := range pkgs {
			if allPkgs.Find(pkg.ImportPath, pkg.Variant) != nil {
				continue
			}

			analyzePkg(pkg)
			allPkgs.Add(pkg)
		}
	}

	for _, pkg := range allPkgs.Array()[:] {
		// We only care about transitive test deps of the stuff we will test
		if pkg.IsPartOfModule {
			pkg.TestDeps = computeTestDepsFromImports(allPkgs, pkg, pkg.TestImports)
			pkg.XTestDeps = computeTestDepsFromImports(allPkgs, pkg, pkg.XTestImports)
		}
	}

	allPkgs.Sort()

	return allPkgs
}

func computeTestDepsFromImports(allPkgs *Packages, pkg *Package, imports []string) []string {
	testDeps := make([]string, 0)

	for _, depPath := range imports {
		if dpkg := allPkgs.Find(depPath, pkg.Variant); dpkg != nil {
			testDeps = append(testDeps, dpkg.ImportPath)
			testDeps = append(testDeps, dpkg.Deps...)
		} else {
			for _, p := range goList(depPath, pkg.Variant) {
				testDeps = append(testDeps, p.ImportPath)

				if allPkgs.Find(p.ImportPath, p.Variant) == nil {
					analyzePkg(p)
					allPkgs.Add(p)
				}
			}
		}
	}

	return testDeps
}
