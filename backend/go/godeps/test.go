package main

import (
	"fmt"
	"io"
	"strings"
	"text/template"
)

type LibTest struct {
	TestLib  *Lib
	XTestLib *Lib

	DepsLibs []string

	ImportPath string
	PreRun     string

	TestFiles  []string
	XTestFiles []string
}

func (t LibTest) Data() interface{} {
	testFilesForAnalysis := make([]string, 0)
	for _, file := range t.TestFiles {
		testFilesForAnalysis = append(testFilesForAnalysis, "_test:"+file)
	}
	for _, file := range t.XTestFiles {
		testFilesForAnalysis = append(testFilesForAnalysis, "_xtest:"+file)
	}

	var variant PkgCfgVariant
	if t.TestLib != nil {
		variant = t.TestLib.Variant
	} else if t.XTestLib != nil {
		variant = t.XTestLib.Variant
	}

	return map[string]interface{}{
		"Config": Config,

		"ImportPath": t.ImportPath,
		"DepsLibs":   genStringArray(t.DepsLibs, 2),

		"PreRun":               t.PreRun,
		"TestFilesForAnalysis": strings.Join(testFilesForAnalysis, " "),

		"TestLib":  RenderLibCall(t.TestLib),
		"XTestLib": RenderLibCall(t.XTestLib),
		"GoFiles":  genStringArray(append(t.TestFiles, t.XTestFiles...), 2),

		"VID":        VID(variant),
		"Variant":    genVariant(variant, false),
		"VariantBin": genVariant(variant, true),
		"IfTest":     fmt.Sprintf("'%v' == get_os() and '%v' == get_arch()", variant.OS, variant.ARCH),
	}
}

var testTplStr = `
# test {{.ImportPath}}

go = "{{.Config.Go}}"
generate_testmain = "{{.Config.GenerateTestMain}}"

load("{{.Config.BackendPkg}}", "go_library")
load("{{.Config.BackendPkg}}", "go_build_bin")

test_lib = {{.TestLib}}
xtest_lib = {{.XTestLib}}

gen_testmain = target(
    name="_go_gen_testmain@{{.VID}}",
    deps={{.GoFiles}},
    run=[
		'generate_testmain {{.ImportPath}} {{.TestFilesForAnalysis}}',
		'mkdir -p testmain && mv _testmain.go $OUT',
	],
    out=['testmain/_testmain.go'],
    tools=[go, generate_testmain],
    env={
        "OS": get_os(),
        "ARCH": get_arch(),
    },
)

testmain_lib = go_library(
	name="_go_testmain_lib@{{.VID}}",
	src_dep=gen_testmain,
	libs=[test_lib, xtest_lib],
	go_files=['_testmain.go'],
	import_path="{{.ImportPath}}/testmain",
	dir="testmain",
	{{.Variant}}
)

test_build = go_build_bin(
    name="go_test#build@{{.VID}}",
	main=testmain_lib,
    libs={{.DepsLibs}},
	out="pkg.test",
	{{.VariantBin}}
)

if {{.IfTest}}:
	target(
		name="go_test@{{.VID}}",
		deps={
			'bin': test_build,
			'data': '$(collect "{}/." include="go_test_data")'.format(heph.pkg.addr()),
		},
		run=[
			{{- if .PreRun}}'{{.PreRun}}',{{end}}
			'./$SRC_BIN -test.v 2>&1 | tee test_out',
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
