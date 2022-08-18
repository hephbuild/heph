package main

import (
	"io"
	"strings"
	"text/template"
)

type LibTest struct {
	ImportPath    string
	TargetPackage string
	Deps          []string
	PreRun        string
	TestFiles     []string
	XTestFiles    []string
	StdPkgs       []string
	Pkgs          []string
}

func (t LibTest) Data() interface{} {
	testFilesForAnalysis := make([]string, 0)
	for _, file := range t.TestFiles {
		testFilesForAnalysis = append(testFilesForAnalysis, "_test:"+file)
	}
	for _, file := range t.XTestFiles {
		testFilesForAnalysis = append(testFilesForAnalysis, "_xtest:"+file)
	}

	return map[string]interface{}{
		"Config":               Config,
		"ImportPath":           t.ImportPath,
		"Deps":                 genArray(t.Deps, 2),
		"Pkgs":                 genArray(t.Pkgs, 3),
		"StdPkgs":              genArray(t.StdPkgs, 3),
		"PreRun":               t.PreRun,
		"TestFilesForAnalysis": strings.Join(testFilesForAnalysis, " "),
	}
}

var testTplStr = `
# test

go = "{{.Config.Go}}"
generate_testmain = "{{.Config.GenerateTestMain}}"

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
	"runtime/cgo",
]

test_build = target(
    name="go_test#build",
    deps={{.Deps}},
    run=[
		'generate_testmain {{.ImportPath}} {{.TestFilesForAnalysis}}',
		'mkdir -p testmain && mv _testmain.go testmain',
	] + compile_cmd("*.go") + compile_cmd("*.go", "testmain") + [
		'echo Linking bin...',
        'go tool link -importcfg "$SANDBOX/importconfig" -o pkg.test testmain/lib.a',
	],
    out=['pkg.test'],
    tools=[go, generate_testmain],
    env={
        "OS": get_os(),
        "ARCH": get_arch(),
        "PKGS": " ".join({{.Pkgs}}),
        "STD_PKGS": " ".join({{.StdPkgs}}+test_std_pkgs),
    },
)

target(
    name="go_test",
    deps=test_build,
    run=[
		'{{.PreRun}} ./pkg.test -test.v 2>&1 | tee test_out',
	],
    out=['test_out'],
    labels=["test"],
)

# end test
`

var testTpl *template.Template

func init() {
	var err error
	testTpl, err = template.New("test").Parse(testTplStr)
	if err != nil {
		panic(err)
	}
}

func RenderTest(w io.Writer, t *LibTest) {
	err := testTpl.Execute(w, t.Data())
	if err != nil {
		panic(err)
	}
}
