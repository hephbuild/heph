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
# mod

go = "{{.Config.Go}}"
godeps = "{{.Config.GoDeps}}"

target(
	name="{{.Target.Name}}",
	run=[
		'echo module heph_ignore > go.mod', # stops go reading the main go.mod, and downloading all of those too
		"go mod download -modcacherw -json {{.Path}}@{{.Version}} | tee mod.json",
		'rm go.mod',
		'export MOD_DIR=$(cat mod.json | awk -F\\" \'/"Dir": / { print $4 }\') && cp -r "$MOD_DIR/." .',
	],
	tools=[go],
	out=["."],
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
