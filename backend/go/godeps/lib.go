package main

import (
	"bytes"
	"fmt"
	"io"
	"strconv"
	"strings"
	"text/template"
)

type Lib struct {
	Target        Target
	Variant       PkgCfgVariant
	ImportPath    string
	ModRoot       string
	Libs          []string
	GoFiles       []string
	SFiles        []string
	EmbedPatterns []string
	SrcDep        string
	GenEmbed      bool
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
		"GenEmbed":   l.GenEmbed,
		"SrcDep":     l.SrcDep,
		"Variant":    genVariant(l.Variant, false),
	}
}

var libCallTplStr = `
go_library(
	name="{{.Target.Name}}",
	import_path="{{.ImportPath}}",{{if .SrcDep}}
	src_dep="{{.SrcDep}}",{{end}}
	libs={{.Libs}},
	go_files={{.GoFiles}},
	s_files={{.SFiles}},
	resources={{.EmbedGlobs}},
	{{if .GenEmbed}}gen_embed=True,{{end}}
	{{.Variant}},
)
`

var libTplStr = `
# lib {{.ImportPath}}

load("{{.Config.BackendPkg}}", "go_library")

{{.Lib}}

# end lib
`

var libCallTpl *template.Template
var libTpl *template.Template

func init() {
	var err error
	libTpl, err = template.New("lib").Parse(libTplStr)
	if err != nil {
		panic(err)
	}
	libCallTpl, err = template.New("lib").Parse(libCallTplStr)
	if err != nil {
		panic(err)
	}
}

func RenderLib(w io.Writer, l *Lib) {
	data := l.Data()

	data["Lib"] = RenderLibCall(l)

	err := libTpl.Execute(w, data)
	if err != nil {
		panic(err)
	}
}

func RenderLibCall(l *Lib) string {
	if l == nil {
		return "None"
	}

	var buf bytes.Buffer
	err := libCallTpl.Execute(&buf, l.Data())
	if err != nil {
		panic(err)
	}

	return strings.TrimSpace(buf.String())
}
