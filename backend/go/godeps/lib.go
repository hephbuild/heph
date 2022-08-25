package main

import (
	"io"
	"text/template"
)

type Lib struct {
	Target     Target
	ImportPath string
	ModRoot    string
	Libs       []string
	GoFiles    []string
	SFiles     []string
	SrcDep     string
}

func (l Lib) Data() map[string]interface{} {
	return map[string]interface{}{
		"Config":     Config,
		"ModRoot":    l.ModRoot,
		"ImportPath": l.ImportPath,
		"Target":     l.Target,
		"Libs":       genArray(l.Libs, 2),
		"GoFiles":    genArray(l.GoFiles, 2),
		"SFiles":     genArray(l.SFiles, 2),
		"SrcDep":     l.SrcDep,
	}
}

var libTplStr = `
# lib {{.ImportPath}}

load("{{.Config.BackendPkg}}", "go_library")

go_library(
	name="{{.Target.Name}}",
	import_path="{{.ImportPath}}",
{{if .SrcDep}}	src_dep="{{.SrcDep}}",{{end}}
	libs={{.Libs}},
	go_files={{.GoFiles}},
	s_files={{.SFiles}},
)

# end lib
`

var libTpl *template.Template

func init() {
	var err error
	libTpl, err = template.New("lib").Parse(libTplStr)
	if err != nil {
		panic(err)
	}
}

func RenderLib(w io.Writer, l *Lib) {
	err := libTpl.Execute(w, l.Data())
	if err != nil {
		panic(err)
	}
}
