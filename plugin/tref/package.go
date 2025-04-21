package tref

import (
	"fmt"
	"path"
	"path/filepath"
	"slices"
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
	s = slices.DeleteFunc(s, func(s string) bool {
		return s == ""
	})
	return strings.Join(s, "/")
}

func DirPackage(s string) string {
	return path.Dir(s)
}

func HasPackagePrefix(pkg, prefix string) bool {
	return pkg == prefix || strings.HasPrefix(pkg, prefix+"/")
}

func CutPackagePrefix(pkg, prefix string) (string, bool) {
	if pkg == prefix {
		return "", true
	}

	return strings.CutPrefix(pkg, prefix+"/")
}

func CutPackage(pkg, prefix string) (string, string, bool) {
	before, after, found := strings.Cut(pkg, prefix)

	before = strings.TrimSuffix(before, "/")
	after = strings.TrimPrefix(after, "/")

	return before, after, found
}
