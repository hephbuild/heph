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
		"PreRun":               t.PreRun,
		"TestFilesForAnalysis": strings.Join(testFilesForAnalysis, " "),
	}
}

var testTplStr = `
# test

go = "{{.Config.Go}}"
generate_testmain = "{{.Config.GenerateTestMain}}"

load("{{.Config.BackendPkg}}", "go_library")
load("{{.Config.BackendPkg}}", "go_bin_link")

test_lib = go_library(
	name="_go_lib_test",
    deps={{.Deps}}+["{{.Config.StdPkgsTarget}}"],
	files="*.go",
	import_path="{{.ImportPath}}",
)

gen_testmain = target(
    name="_go_gen_testmain",
    deps={{.Deps}}+["{{.Config.StdPkgsTarget}}"],
    run=[
		'generate_testmain {{.ImportPath}} {{.TestFilesForAnalysis}}',
		'mkdir -p testmain && mv _testmain.go testmain',
	],
    out=['testmain'],
    tools=[go, generate_testmain],
    env={
        "OS": get_os(),
        "ARCH": get_arch(),
    },
)

testmain_lib = go_library(
	name="_go_testmain_lib",
    deps={{.Deps}}+["{{.Config.StdPkgsTarget}}", test_lib, gen_testmain],
	files="*.go",
	import_path="{{.ImportPath}}/testmain",
	dir="testmain",
)

test_build = go_bin_link(
    name="go_test#build",
    deps={{.Deps}}+["{{.Config.StdPkgsTarget}}", test_lib, testmain_lib],
	path="testmain/lib.a",
	out="pkg.test",
)

target(
    name="go_test",
    deps=test_build,
    run='{{.PreRun}} ./pkg.test -test.v 2>&1 | tee test_out',
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
