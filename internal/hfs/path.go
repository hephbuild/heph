package hfs

import (
	"iter"
	"path/filepath"
	"strings"
)

// Equivalent to `strings.HasPrefix(path, prefix+"/")`, without the string concat.
func HasPathPrefix(path, prefix string) bool {
	return path == prefix || len(path) >= len(prefix) &&
		strings.HasPrefix(path, prefix) &&
		path[len(prefix)] == '/'
}

func ParentPaths(p string) iter.Seq[string] {
	return func(yield func(string) bool) {
		for p != "" {
			if !yield(p) {
				return
			}

			p, _ = filepath.Split(p)
			p = strings.TrimSuffix(p, string(filepath.Separator))

			if p == "" {
				break
			}
		}

		yield("")
	}
}
