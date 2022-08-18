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
	"strings"
)

type Mod struct {
	Main    bool
	Path    string
	Dir     string
	Version string
	Replace *Mod
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

	// Part of our own module
	IsPartOfModule bool     `json:"-"`
	TestDeps       []string `json:"-"`
}

type Packages []*Package

func (p Packages) Find(importPath string) *Package {
	for _, p := range p {
		if p.ImportPath == importPath {
			return p
		}
	}

	return nil
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

func init() {
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

func goList(pkg string) Packages {
	cmd := exec.Command("go", "list", "-e", "-json", "-deps", pkg)

	b, err := cmd.Output()
	if err != nil {
		var eerr *exec.ExitError
		if errors.As(err, &eerr) {
			panic(string(eerr.Stderr))
		}
		panic(err)
	}

	pkgs := make(Packages, 0)

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

		pkgs = append(pkgs, &pkg)
	}

	return pkgs
}

func goListWithTransitiveTestDeps() Packages {
	pkgs := goList("./...")

	for _, pkg := range pkgs[:] {
		var modrel string
		if pkg.Module != nil {
			modrel, _ = filepath.Rel(os.Getenv("SANDBOX"), pkg.Module.Dir)
		}

		pkg.IsPartOfModule = len(modrel) > 0 && !strings.Contains(modrel, "..")

		if pkg.IsPartOfModule {
			// We only care about transitive test deps of the stuff we will test

			testDeps := make([]string, 0)

			for _, depPath := range pkg.TestImports {
				testDeps = append(testDeps, depPath)
				for _, p := range goList(depPath) {
					testDeps = append(testDeps, p.ImportPath)

					if pkgs.Find(p.ImportPath) == nil {
						pkgs = append(pkgs, p)
					}
				}
			}

			pkg.TestDeps = testDeps
		}
	}

	return pkgs
}
