package pluginexec

import (
	"bytes"
	_ "embed"
	"strings"
	"text/template"
)

//go:embed initfile.sh
var initfile string
var initfiletpl *template.Template

func init() {
	var err error
	initfiletpl = template.New("init")
	initfiletpl, err = initfiletpl.Parse(initfile)
	if err != nil {
		panic(err)
	}
}

func RenderInitFile(cmds string) (string, error) {
	var buf bytes.Buffer
	err := initfiletpl.Execute(&buf, map[string]string{
		"Cmds":    cmds,
		"CmdsStr": strings.ReplaceAll(cmds, "'", `'\''`),
	})
	if err != nil {
		return "", err
	}

	return buf.String(), nil
}
