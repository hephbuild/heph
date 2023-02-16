package main

import (
	"io"
	"text/template"
)

type Bin struct {
	TargetName    string
	ImportPath    string
	TargetPackage string

	MainLib string
	Libs    []string
	Variant PkgCfgVariant
}

func (l Bin) Data() map[string]interface{} {
	return map[string]interface{}{
		"Config":     Config,
		"ImportPath": l.ImportPath,
		"Name":       l.TargetName,
		"Libs":       genStringArray(l.Libs, 2),
		"MainLib":    l.MainLib,
		"Variant":    genVariant(l.Variant, true, true),
	}
}

var binTplStr = `
# bin {{.ImportPath}}

load("{{.Config.BackendPkg}}", "go_build_bin")

go_build_bin(
	name="{{.Name}}",
	libs={{.Libs}},
	main="{{.MainLib}}",
	{{.Variant}},
)

# end bin
`

var binTpl *template.Template

func init() {
	var err error
	binTpl, err = template.New("bin").Parse(binTplStr)
	if err != nil {
		panic(err)
	}
}

func RenderBin(w io.Writer, l *Bin) {
	err := binTpl.Execute(w, l.Data())
	if err != nil {
		panic(err)
	}
}
