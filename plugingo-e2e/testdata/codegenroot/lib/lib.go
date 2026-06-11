package lib

import "codegenroot/genpb"

// Greeting consumes the generated package, forcing a cross-package
// build_lib -> _golist resolution against the generated sub-package.
func Greeting() string {
	return genpb.GeneratedVar
}
