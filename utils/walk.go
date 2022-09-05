package utils

import (
	"errors"
	"github.com/bmatcuk/doublestar/v4"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
)

func isIgnored(path string, ignored []string) bool {
	parts := strings.Split(path, string(filepath.Separator))

	for _, i := range ignored {
		if strings.HasPrefix(i, string(filepath.Separator)) {
			if strings.HasPrefix(path, strings.TrimLeft(i, "/")) {
				return true
			}
		} else {
			for _, p := range parts {
				if i == p {
					return true
				}
			}
		}
	}

	return false
}

// From doublestar package

var metaReplacer = strings.NewReplacer("\\*", "*", "\\?", "?", "\\[", "[", "\\]", "]", "\\{", "{", "\\}", "}")

// Unescapes meta characters (*?[]{})
func unescapeMeta(pattern string) string {
	return metaReplacer.Replace(pattern)
}

func indexMeta(s string) int {
	var c byte
	l := len(s)
	for i := 0; i < l; i++ {
		c = s[i]
		if c == '*' || c == '?' || c == '[' || c == '{' {
			return i
		} else if c == '\\' {
			// skip next byte
			i++
		}
	}
	return -1
}

func StarWalk(root, pattern string, ignore []string, fn fs.WalkDirFunc) error {
	i := indexMeta(pattern)

	if i == -1 {
		rel := unescapeMeta(pattern)
		abs := filepath.Join(root, rel)
		info, err := os.Stat(abs)
		if err != nil {
			if errors.Is(err, fs.ErrNotExist) {
				return nil
			}

			return err
		}

		return fn(rel, fs.FileInfoToDirEntry(info), nil)
	}

	walkRoot := root
	if i > 0 {
		walkRoot = filepath.Join(root, unescapeMeta(pattern[:i]))
		pattern = pattern[i:]
	}

	return filepath.WalkDir(walkRoot, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}

		rel, err := filepath.Rel(root, path)
		if err != nil {
			return err
		}

		if isIgnored(rel, ignore) {
			if d.IsDir() {
				return filepath.SkipDir
			} else {
				return nil
			}
		}

		ok, err := doublestar.PathMatch(pattern, rel)
		if err != nil {
			return err
		}

		if ok {
			err := fn(rel, d, err)
			if err != nil {
				return err
			}
		}

		return nil
	})
}

func StarWalkAbs(root, pattern string, ignore []string, fn fs.WalkDirFunc) error {
	return StarWalk(root, pattern, ignore, func(path string, d fs.DirEntry, err error) error {
		return fn(filepath.Join(root, path), d, err)
	})
}
