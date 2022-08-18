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
# lib

go = "{{.Config.Go}}"

gen_importcfg = [
	'echo $SANDBOX',
	'cat "$SANDBOX/{{.Config.StdPkgsListFile}}" | xargs -I{} echo "packagefile {}=$GO_OUTDIR/go/pkg/${OS}_${ARCH}/{}.a" | sort -u > $SANDBOX/importconfig',
	'find "$SANDBOX" -name "importcfg" | xargs -I{} cat {} | sed -e "s:=:=$SANDBOX/:" | sort -u >> $SANDBOX/importconfig'
]

def compile_cmd(files, dir=None):
    return gen_importcfg + [
		'echo "Compiling ({})..."'.format(dir),
        ('cd '+dir+' && ' if dir else '') + 'go tool compile -importcfg $SANDBOX/importconfig -trimpath "$ROOT" -o lib.a -pack '+files,
		'echo "packagefile {{.ImportPath}}=$PACKAGE/lib.a" > importcfg',
    ]

target(
    name="{{.Target.Name}}",
    deps={{.Deps}}+["{{.Config.StdPkgsTarget}}"],
    run=compile_cmd("{{.CompileFiles}}"),
    out=['lib.a', 'importcfg'],
    tools=[go],
    env={
        "OS": get_os(),
        "ARCH": get_arch(),
    },
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
