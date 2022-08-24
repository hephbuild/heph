package main

import (
	"io"
	"text/template"
)

type ModDl struct {
	Target  Target
	Path    string
	Version string
}

func (m ModDl) Data() interface{} {
	return map[string]interface{}{
		"Config":  Config,
		"Target":  m.Target,
		"Path":    m.Path,
		"Version": m.Version,
	}
}

var modTplStr = `
# mod {{.Path}}@{{.Version}}

load("{{.Config.BackendPkg}}", "go_mod_download")

go_mod_download(
	name="{{.Target.Name}}",
	path="{{.Path}}",
	version="{{.Version}}",
)

# end mod
`

var modTpl *template.Template

func init() {
	var err error
	modTpl, err = template.New("mod").Parse(modTplStr)
	if err != nil {
		panic(err)
	}
}

func RenderModDl(w io.Writer, m *ModDl) {
	err := modTpl.Execute(w, m.Data())
	if err != nil {
		panic(err)
	}
}
