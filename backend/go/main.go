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
	"strconv"
	"strings"
	"text/template"
)

func fatal(err ...interface{}) {
	fmt.Println(append([]interface{}{"FATAL:"}, err...))
	os.Exit(1)
}

var modTemplate = `
go = "{{.Config.Go}}"
godeps = "{{.Config.GoDeps}}"

target(
	name="{{.TargetName}}#download",
	run=[
		'echo module please_ignore > go.mod', # stops go reading the main go.mod, and downloading all of those too
		"export GOPATH=$(pwd)/gopath && go mod download -json {{.Path}}@{{.Version}} | tee mod.json",
		'export MOD_DIR=$(cat mod.json | awk -F\\" \'/"Dir": / { print $4 }\') && cp -r "$MOD_DIR" "{{.Path}}"',
	],
	tools=[go],
	out=["{{.Path}}", 'mod.json'],
)

target(
	name="_{{.TargetName}}#gen",
	run=[
		"godeps \"$TARGET\" 3rdparty",
	],
	tools=[go, godeps],
	out="/**/BUILD.gen",
	gen=true,
)
`

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
}

type PackageDep struct {
	Package

	LibTarget     string
	TestLibTarget string
}

type ModDep struct {
	Mod
	TargetName string
}

var pkgTemplate = `
go = "{{.Config.Go}}"
generate_testmain = "{{.Config.GenerateTestMain}}"

gen_importcfg = [
	'echo $ROOT',
    'echo "$STD_PKGS" | tr " " "\n" | xargs -I{} echo "packagefile {}=$GO_OUTDIR/go/pkg/${OS}_${ARCH}/{}.a" | sort -u > $ROOT/importconfig',
    'echo "$PKGS" | tr " " "\n" | xargs -I{} echo "packagefile {}=$ROOT/{{.ModRoot}}/{}/lib.a" | sort -u >> $ROOT/importconfig',
]

def compile_cmd(dir=None):
    return gen_importcfg + [
		'echo "Compiling ({})..."'.format(dir),
        ('cd '+dir+' && ' if dir else '') + 'go tool compile -importcfg $ROOT/importconfig -trimpath "$ROOT" -o lib.a -pack *.go',
    ]

target(
    name="_go_lib",
    deps={{.Lib.Deps}},
    run=compile_cmd(),
    out=['lib.a'],
    tools=[go],
    env={
        "OS": get_os(),
        "ARCH": get_arch(),
        "PKGS": " ".join({{.Lib.Pkgs}}),
        "STD_PKGS": " ".join({{.Lib.StdPkgs}}),
    },
)

test_std_pkgs = [
	"os",
	"reflect",
	"testing",
	"testing/internal/testdeps",
    "flag",
    "internal/sysinfo",
    "strings",
    "bytes",
    "path/filepath",
    "math/rand",
    "runtime/debug",
    "runtime/trace",
    "bufio",
    "context",
    "internal/fuzz",
    "os/signal",
    "regexp",
    "runtime/pprof",
	"go/ast",
	"go/parser",
	"go/token",
	"crypto/sha256",
	"internal/godebug",
	"io/ioutil",
	"encoding/binary",
	"os/exec",
	"encoding/json",
	"regexp/syntax",
	"text/tabwriter",
	"compress/gzip",
	"go/scanner",
	"go/internal/typeparams",
	"crypto",
	"hash",
	"encoding",
	"encoding/base64",
	"unicode/utf16",
	"compress/flate",
	"hash/crc32",
]

{{ if .HasTests }}
test_build = target(
    name="go_test#build",
    deps={{.TestLib.Deps}},
    run=[
		'generate_testmain {{.Pkg.ImportPath}} {{.TestFilesForAnalysis}}',
		'mkdir -p testmain && mv _testmain.go testmain',
	] + compile_cmd() + compile_cmd("testmain") + [
		'echo Linking bin...',
        '{{.TestPreRun}} go tool link -importcfg $ROOT/importconfig -o pkg.test testmain/lib.a',
	],
    out=['pkg.test'],
    tools=[go, generate_testmain],
    env={
        "OS": get_os(),
        "ARCH": get_arch(),
        "PKGS": " ".join({{.TestLib.Pkgs}}),
        "STD_PKGS": " ".join({{.Test.StdPkgs}}+test_std_pkgs),
    },
)

target(
    name="go_test",
    deps=test_build,
    run=[
		'./pkg.test -test.v 2>&1 | tee test_out',
	],
    out=['test_out'],
    labels=["test"],
)
{{ end }}
`

func targetName(mod Mod) string {
	return fmt.Sprintf("_%v#%v", strings.ReplaceAll(mod.Path, "/", "_"), mod.Version)
}

func genArray(es []string, l int) string {
	if len(es) == 0 {
		return "[]"
	}

	m := map[string]struct{}{}

	var sb strings.Builder
	sb.WriteString("[\n")
	for _, e := range es {
		if _, ok := m[e]; ok {
			continue
		}
		m[e] = struct{}{}

		for i := 0; i < l; i++ {
			sb.WriteString("    ")
		}
		sb.WriteString(strconv.Quote(e))
		sb.WriteString(",\n")
	}
	for i := 0; i < l-1; i++ {
		sb.WriteString("    ")
	}
	sb.WriteString("]")
	return sb.String()
}

type DepsSpec struct {
	Deps    []string
	Pkgs    []string
	StdPkgs []string
}

func (d *DepsSpec) DepPkg(dep *PackageDep) {
	if dep.LibTarget == "" {
		panic(fmt.Errorf("%v does not have a libtarget", dep.ImportPath))
	}

	d.Deps = append(d.Deps, dep.LibTarget)
	d.Pkgs = append(d.Pkgs, dep.ImportPath)
}

func (d *DepsSpec) Data() interface{} {
	sort.Strings(d.Deps)
	sort.Strings(d.Pkgs)
	sort.Strings(d.StdPkgs)

	return map[string]interface{}{
		"Deps":    genArray(d.Deps, 2),
		"Pkgs":    genArray(d.Pkgs, 3),
		"StdPkgs": genArray(d.StdPkgs, 3),
	}
}

func genPkgLib(root string, cfg Cfg, pkg PackageDep, pkgToDep map[string]*PackageDep) {
	pkgTpl, err := template.New("dep").Parse(pkgTemplate)
	if err != nil {
		panic(err)
	}

	rel, err := filepath.Rel(root, pkg.Dir)
	if err != nil {
		fatal(fmt.Errorf("%#v : %v", pkg, err))
	}

	buildFile, err := os.OpenFile(filepath.Join(root, rel, "BUILD.gen"), os.O_RDWR|os.O_CREATE, os.ModePerm)
	if err != nil {
		fatal(err)
	}
	defer buildFile.Close()

	libDeps := &DepsSpec{}
	testLibDeps := &DepsSpec{}
	testDeps := &DepsSpec{}

	libDeps.Deps = append(libDeps.Deps, pkg.GoFiles...)

	for _, depPath := range pkg.Imports {
		if depPath == pkg.ImportPath {
			continue
		}

		if depPath == "builtin" || depPath == "unsafe" {
			continue
		}

		if isStandard(depPath) {
			libDeps.StdPkgs = append(libDeps.StdPkgs, depPath)
			testLibDeps.StdPkgs = append(testLibDeps.StdPkgs, depPath)
			continue
		}

		dep := pkgToDep[depPath]
		if dep == nil {
			fatal(fmt.Errorf("Imports: dep not found %v %v %v", pkg.Dir, depPath, pkgToDep))
		}

		testLibDeps.DepPkg(dep)
		libDeps.DepPkg(dep)
	}

	for _, depPath := range pkg.TestImports {
		if depPath == pkg.ImportPath {
			continue
		}

		if depPath == "builtin" || depPath == "unsafe" {
			continue
		}

		if isStandard(depPath) {
			testLibDeps.StdPkgs = append(testLibDeps.StdPkgs, depPath)
			continue
		}

		dep := pkgToDep[depPath]
		if dep == nil {
			fatal(fmt.Errorf("TestImports: dep not found %v %v %v", pkg.Dir, depPath, pkgToDep))
		}

		testLibDeps.DepPkg(dep)
	}

	for _, depPath := range pkg.Deps {
		if depPath == "builtin" || depPath == "unsafe" {
			continue
		}

		if isStandard(depPath) {
			testDeps.StdPkgs = append(testDeps.StdPkgs, depPath)
			continue
		}

		dep := pkgToDep[depPath]
		if dep == nil {
			fatal(fmt.Errorf("Deps: dep not found %v %v %v", pkg.Dir, depPath, pkgToDep))
		}

		testDeps.DepPkg(dep)
	}

	dep := pkgToDep[pkg.ImportPath]
	if dep == nil {
		fatal(fmt.Errorf("mod not found %v %v %v", pkg.Dir, pkg.ImportPath, pkgToDep))
	}

	testFiles := make([]string, 0)
	testFiles = append(testFiles, dep.GoFiles...)
	testFiles = append(testFiles, dep.TestGoFiles...)
	testFiles = append(testFiles, dep.XTestGoFiles...)

	testFilesForAnalysis := make([]string, 0)
	for _, file := range dep.TestGoFiles {
		testFilesForAnalysis = append(testFilesForAnalysis, "_test:"+file)
	}
	for _, file := range dep.XTestGoFiles {
		testFilesForAnalysis = append(testFilesForAnalysis, "_xtest:"+file)
	}

	testLibDeps.Deps = append(testLibDeps.Deps, testFiles...)
	testLibDeps.DepPkg(dep)

	isInTarget := dep.Module != nil && dep.Module.Main

	testDeps.Deps = append(testDeps.Pkgs, pkg.TestLibTarget)
	testDeps.Deps = append(testDeps.Deps, testFiles...)

	err = pkgTpl.Execute(buildFile, map[string]interface{}{
		"Pkg":                  dep,
		"Config":               cfg,
		"ModRoot":              filepath.Dir(os.Getenv("PACKAGE")),
		"Lib":                  libDeps.Data(),
		"TestLib":              testLibDeps.Data(),
		"HasTests":             isInTarget && !cfg.IsTestSkipped(rel) && len(testFiles) > 0,
		"Test":                 testDeps.Data(),
		"TestPreRun":           cfg.Test.PreRun,
		"TestFilesForAnalysis": strings.Join(testFilesForAnalysis, " "),
	})
	if err != nil {
		fatal(err)
	}
}

func isStandard(importPath string) bool {
	p := filepath.Join(os.Getenv("GO_OUTDIR"), "go/pkg", os.Getenv("OS")+"_"+os.Getenv("ARCH"), importPath+".a")

	_, err := os.Lstat(p)

	return err == nil
}

func genPkgDeps(root string, cfg Cfg, isThirdparty bool) {
	modTpl, err := template.New("mod").Parse(modTemplate)
	if err != nil {
		panic(err)
	}

	fmt.Println("go list ./...")
	cmd := exec.Command("go", "list", "-e", "-json", "-deps", "./...")

	b, err := cmd.Output()
	if err != nil {
		var eerr *exec.ExitError
		if errors.As(err, &eerr) {
			fatal(string(eerr.Stderr))
		}
		fatal(err)
	}

	fmt.Println(string(b))

	pkgToDep := map[string]*PackageDep{}
	pkgs := []PackageDep{}
	thirdpartym := map[string]struct{}{}
	thirdparty := []ModDep{}

	dec := json.NewDecoder(bytes.NewReader(b))
	for {
		var pkg Package

		err := dec.Decode(&pkg)
		if err == io.EOF {
			// all done
			break
		}
		if err != nil {
			fatal(err)
		}

		dep := PackageDep{
			Package: pkg,
		}

		if pkg.Standard {
			pkgToDep[pkg.ImportPath] = &dep
			continue
		}

		var modrel string
		if pkg.Module != nil {
			modrel, _ = filepath.Rel(os.Getenv("SANDBOX"), pkg.Module.Dir)
		}

		if len(modrel) > 0 && !strings.Contains(modrel, "..") {
			rel, _ := filepath.Rel(os.Getenv("SANDBOX"), pkg.Dir)

			dep.LibTarget = "//" + rel + ":_go_lib"
			dep.TestLibTarget = "//" + rel + ":_go_lib_test"
		} else {
			if dep.Module == nil {
				fatal("%v does not have a module", dep.ImportPath)
			}
			mod := *dep.Module

			dep.LibTarget = "//thirdparty/go:" + targetName(mod) + ":_go_lib"
			dep.TestLibTarget = "//thirdparty/go:" + targetName(mod) + ":_go_lib_test"

			if _, ok := thirdpartym[mod.Path]; !ok {
				thirdparty = append(thirdparty, ModDep{
					Mod:        mod,
					TargetName: targetName(mod),
				})
				thirdpartym[mod.Path] = struct{}{}
			}
		}

		pkgs = append(pkgs, dep)
		pkgToDep[pkg.ImportPath] = &dep
	}

	tpDir := filepath.Join(root, "thirdparty", "go")
	err = os.MkdirAll(tpDir, os.ModePerm)
	if err != nil {
		panic(err)
	}

	depsFile, err := os.Create(filepath.Join(tpDir, "BUILD.gen"))
	if err != nil {
		panic(err)
	}
	defer depsFile.Close()

	sort.SliceStable(thirdparty, func(i, j int) bool {
		return thirdparty[i].Path < thirdparty[j].Path
	})

	for _, mod := range thirdparty {
		err = modTpl.Execute(depsFile, mod)
		if err != nil {
			fatal(err)
		}
	}

	for _, pkg := range pkgs {
		genPkgLib(root, cfg, pkg, pkgToDep)
	}
}

type Cfg struct {
	Test struct {
		Skip   []string `json:"skip"`
		PreRun string   `json:"pre_run"`
	} `json:"test"`
	Go               string `json:"go"`
	GoDeps           string `json:"godeps"`
	GenerateTestMain string `json:"generate_testmain"`
}

func (c Cfg) IsTestSkipped(pkg string) bool {
	for _, s := range c.Test.Skip {
		if s == pkg {
			return true
		}
	}

	return false
}

var (
	target = os.Args[1]
	mode   = os.Args[2]
)

func main() {
	fmt.Println("Hello")

	root := os.Getenv("SANDBOX")
	if root == "" {
		fatal("SANDBOX is missing")
	}

	var cfg Cfg
	if len(os.Args) > 1 {
		cfgs := os.Args[3]
		if len(cfgs) > 1 {
			err := json.Unmarshal([]byte(cfgs), &cfg)
			if err != nil {
				fatal(err)
				os.Exit(1)
			}
		}
	}

	genPkgDeps(root, cfg, mode == "3rdparty")
}
