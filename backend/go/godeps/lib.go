package main

import (
	"fmt"
	"io"
	"strconv"
	"text/template"
)

type Lib struct {
	Target        Target
	ImportPath    string
	ModRoot       string
	Libs          []string
	GoFiles       []string
	SFiles        []string
	EmbedPatterns []string
	SrcDep        string
}

func EmbedPatternsGlobs(ps []string) string {
	embedGlobs := make([]string, 0, len(ps))
	for _, p := range ps {
		embedGlobs = append(embedGlobs, fmt.Sprintf(`glob(%v)`, strconv.Quote(p)))
	}

	return joinedArrays(embedGlobs)
}

func (l Lib) Data() map[string]interface{} {
	return map[string]interface{}{
		"Config":     Config,
		"ModRoot":    l.ModRoot,
		"ImportPath": l.ImportPath,
		"Target":     l.Target,
		"Libs":       genStringArray(l.Libs, 2),
		"GoFiles":    genStringArray(l.GoFiles, 2),
		"SFiles":     genStringArray(l.SFiles, 2),
		"EmbedGlobs": EmbedPatternsGlobs(l.EmbedPatterns),
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
	resources={{.EmbedGlobs}},
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
