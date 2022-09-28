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

type Package struct {
	Dir            string
	Root           string
	Name           string
	ImportPath     string
	Module         *Mod
	Standard       bool
	Deps           []string
	Imports        []string
	TestImports    []string
	GoFiles        []string
	IgnoredGoFiles []string
	TestGoFiles    []string
	XTestGoFiles   []string
	CFiles         []string
	CXXFiles       []string
	SFiles         []string

	// Part of our own module
	IsPartOfModule bool     `json:"-"`
	IsPartOfTree   bool     `json:"-"`
	TestDeps       []string `json:"-"`

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
		return p.a[i].ImportPath < p.a[j].ImportPath
	})
}

func (p *Packages) Add(pkg *Package) {
	if p.m == nil {
		p.m = map[string]*Package{}
	}
	p.m[pkg.ImportPath] = pkg
	p.a = append(p.a, pkg)
}

func (p *Packages) MustFind(importPath string) *Package {
	pkg := p.Find(importPath)
	if pkg == nil {
		fmt.Println("unable to find packfor for module", importPath)
		os.Exit(1)
	}

	return pkg
}

func (p *Packages) Find(importPath string) *Package {
	return p.m[importPath]
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

var StdPackages Strings

func InitStd() {
	StdPackages = goListStd()
}

func goListStd() Strings {
	log.Debug("go list std")
	cmd := exec.Command("go", "list", "std")

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

func goList(pkg string) *Packages {
	cmd := exec.Command("go", "list", "-e", "-json", "-deps", pkg)

	b, err := cmd.Output()
	if err != nil {
		var eerr *exec.ExitError
		if errors.As(err, &eerr) {
			panic(string(eerr.Stderr))
		}
		panic(err)
	}

	pkgs := &Packages{}

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

		pkgs.Add(&pkg)
	}

	return pkgs
}

func isPathUnder(path, under string) bool {
	rel, _ := filepath.Rel(under, path)
	return len(rel) > 0 && !strings.Contains(rel, "..")
}

func goListWithTransitiveTestDeps() *Packages {
	pkgs := goList("./...")

	for _, pkg := range pkgs.Array()[:] {
		if pkg.Module != nil {
			partOfTree := isPathUnder(pkg.Module.Dir, Env.Root)
			thirdparty := isPathUnder(pkg.Module.Dir, Env.GOPATH)

			pkg.IsPartOfTree = partOfTree && !thirdparty
		}

		if pkg.IsPartOfTree {
			pkg.IsPartOfModule = isPathUnder(pkg.Dir, filepath.Join(Env.Root, Env.Package))
		}

		if !StdPackages.Includes(pkg.ImportPath) {
			log.Debugln(pkg.Dir)
			log.Debugf("  IsPartOfTree: %v IsPartOfModule: %v\n", pkg.IsPartOfTree, pkg.IsPartOfModule)
		}

		if pkg.IsPartOfModule {
			// We only care about transitive test deps of the stuff we will test

			testDeps := make([]string, 0)

			for _, depPath := range pkg.TestImports {
				testDeps = append(testDeps, depPath)
				for _, p := range goList(depPath).Array() {
					testDeps = append(testDeps, p.ImportPath)

					if pkgs.Find(p.ImportPath) == nil {
						pkgs.Add(p)
					}
				}
			}

			pkg.TestDeps = testDeps
		}
	}

	pkgs.Sort()

	return pkgs
}
