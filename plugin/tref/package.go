package tref

import (
	"fmt"
	"path/filepath"
	"strings"
)

func DirToPackage(dir, hephroot string) (string, error) {
	if !filepath.IsAbs(dir) {
		return "", fmt.Errorf("dir must be abs")
	}

	if !filepath.IsAbs(hephroot) {
		return "", fmt.Errorf("hephroot must be abs")
	}

	if rest, ok := strings.CutPrefix(dir, hephroot); ok {
		return ToPackage(strings.TrimLeft(rest, string(filepath.Separator))), nil
	} else {
		return "", fmt.Errorf("%v not in heph root (%v)", dir, hephroot)
	}
}

func ToOSPath(s string) string {
	return strings.ReplaceAll(s, "/", string(filepath.Separator))
}

func ToPackage(s string) string {
	return strings.ReplaceAll(s, string(filepath.Separator), "/")
}

func SplitPackage(s string) []string {
	return strings.Split(s, "/")
}

func JoinPackage(s ...string) string {
	return strings.Join(s, "/")
}
