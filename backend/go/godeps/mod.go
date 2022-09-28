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

var modDlTplStr = `
# mod {{.Path}}@{{.Version}}

load("{{.Config.BackendPkg}}", "go_mod_download")

go_mod_download(
	name="{{.Target.Name}}",
	path="{{.Path}}",
	version="{{.Version}}",
)

# end mod
`

var modDlTpl *template.Template

func init() {
	var err error
	modDlTpl, err = template.New("mod").Parse(modDlTplStr)
	if err != nil {
		panic(err)
	}
}

func RenderModDl(w io.Writer, m *ModDl) {
	err := modDlTpl.Execute(w, m.Data())
	if err != nil {
		panic(err)
	}
}
