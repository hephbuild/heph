package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
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

func (m *Mod) Actual() *Mod {
	if m.Replace != nil {
		return m.Replace
	}

	return m
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
	fmt.Println("go list std")
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

	fmt.Println(s)

	return strings.Split(s, "\n")
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

	cwd, _ := os.Getwd()
	fmt.Println("### go list ", pkg, cwd)
	fmt.Println(string(b))

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

func goListWithTransitiveTestDeps() *Packages {
	pkgs := goList("./...")

	for _, pkg := range pkgs.Array()[:] {
		if pkg.Module != nil {
			var rel string
			if pkg.Module != nil {
				rel, _ = filepath.Rel(Env.Sandbox, pkg.Module.Dir)
			}
			pkg.IsPartOfTree = len(rel) > 0 && !strings.Contains(rel, "..")
		}

		if pkg.IsPartOfTree {
			var rel string
			if pkg.Module != nil {
				rel, _ = filepath.Rel(filepath.Join(Env.Sandbox, Env.Package), pkg.Dir)
			}
			pkg.IsPartOfModule = len(rel) > 0 && !strings.Contains(rel, "..")
		}

		if !StdPackages.Includes(pkg.ImportPath) {
			fmt.Println(pkg.Dir)
			fmt.Printf("Attr IsPartOfTree: %v IsPartOfModule: %v\n", pkg.IsPartOfTree, pkg.IsPartOfModule)
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
