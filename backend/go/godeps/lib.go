package main

import (
	"io"
	"strings"
	"text/template"
)

type Lib struct {
	Target       Target
	ImportPath   string
	ModRoot      string
	Deps         []string
	CompileFiles []string
}

func (l Lib) Data() map[string]interface{} {
	return map[string]interface{}{
		"Config":       Config,
		"ModRoot":      l.ModRoot,
		"ImportPath":   l.ImportPath,
		"Target":       l.Target,
		"Deps":         genArray(l.Deps, 2),
		"CompileFiles": strings.Join(l.CompileFiles, " "),
	}
}

var libTplStr = `
# lib {{.ImportPath}}

load("{{.Config.BackendPkg}}", "go_library")

go_library(
	name="{{.Target.Name}}",
	deps={{.Deps}}+["{{.Config.StdPkgsTarget}}"],
	files="{{.CompileFiles}}",
	import_path="{{.ImportPath}}",
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
