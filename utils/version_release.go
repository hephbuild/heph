//go:build release

package utils

import (
	_ "embed"
	"strings"
)

//go:embed version
var rawVersion string

var version = strings.TrimSpace(rawVersion)
