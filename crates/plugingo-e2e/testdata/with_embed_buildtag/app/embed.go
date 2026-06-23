//go:build linux
// +build linux

package app

import _ "embed"

//go:embed resources/script.sh
var Script string

func Get() string { return Script }
