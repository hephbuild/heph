package embedgen

import _ "embed"

//go:embed resources/gen.sh
var Script string

func Get() string { return Script }
