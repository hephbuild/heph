package main

import (
	"io"
	"strings"
	"text/template"
)

type GodepsLib struct {
}

func (m GodepsLib) Data() interface{} {
	deps := make([]string, 0)
	for _, dep := range Deps["deps"] {
		if !strings.HasPrefix(dep, "//") {
			dep = "/" + dep
		}

		deps = append(deps, dep)
	}

	return map[string]interface{}{
		"Config": Config,
		"Deps":   genStringArray(deps, 2),
		"Cfg":    genStringArray(Deps["cfg"], 2),
	}
}

var godepsLibTplStr = `
# pkg

load("{{.Config.BackendPkg}}", "go_pkg")

go_pkg(
	deps={{.Deps}},
	cfg={{.Cfg}},
)

# end pkg
`

var godepsLibTpl *template.Template

func init() {
	var err error
	godepsLibTpl, err = template.New("mod").Parse(godepsLibTplStr)
	if err != nil {
		panic(err)
	}
}

func RenderGodepsLib(w io.Writer, m *GodepsLib) {
	err := godepsLibTpl.Execute(w, m.Data())
	if err != nil {
		panic(err)
	}
}
