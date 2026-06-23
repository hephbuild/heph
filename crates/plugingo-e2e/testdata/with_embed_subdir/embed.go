package embedsubdir

import _ "embed"

//go:embed resources/script.sh
var Script string

// Use Script so the variable isn't dead-code eliminated.
func Get() string { return Script }
