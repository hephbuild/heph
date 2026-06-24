package embedgroup

import _ "embed"

//go:embed resources/script.sh
var Script string

func Get() string { return Script }
