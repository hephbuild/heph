package utils

import (
	"errors"
	"github.com/bmatcuk/doublestar/v4"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
)

// Equivalent to `strings.HasPrefix(path, prefix+"/")`, without the string concat
func matchPrefix(path, prefix string) bool {
	return len(path) >= len(prefix) &&
		strings.HasPrefix(path, prefix) &&
		path[len(prefix)] == '/'
}

func fastMatchDir(path, matcher string) bool {
	i := indexMeta(matcher)
	if i == -1 {
		if path == matcher || matchPrefix(path, matcher) {
			return true
		}
	}

	return false
}

func PathMatchAny(path string, matchers ...string) (bool, error) {
	for _, matcher := range matchers {
		if strings.HasSuffix(matcher, "/**/*") {
			matcher := strings.TrimSuffix(matcher, "/**/*")

			if fastMatchDir(path, matcher) {
				return true, nil
			}
		}

		if fastMatchDir(path, matcher) {
			return true, nil
		}

		match, err := doublestar.PathMatch(matcher, path)
		if match || err != nil {
			return match, err
		}
	}

	return false, nil
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

func IsGlob(path string) bool {
	return indexMeta(path) != -1
}

func StarWalk(root, pattern string, ignore []string, fn fs.WalkDirFunc) error {
	i := indexMeta(pattern)

	alwaysMatch := false
	walkRoot := root

	if i == -1 {
		// Pattern is actually a pure path

		rel := unescapeMeta(pattern)
		abs := filepath.Join(root, rel)
		info, err := os.Lstat(abs)
		if err != nil {
			if errors.Is(err, fs.ErrNotExist) {
				return nil
			}

			return err
		}

		if !info.IsDir() {
			// It's not a directory, no need to walk
			return fn(rel, fs.FileInfoToDirEntry(info), nil)
		}

		// All files recursively in the dir would match
		alwaysMatch = true
		walkRoot = abs
	} else if i > 0 {
		i := strings.LastIndex(pattern[:i], string(filepath.Separator))
		if i > 0 {
			p := unescapeMeta(pattern[:i])
			walkRoot = filepath.Join(root, p)
		}
	}

	return filepath.WalkDir(walkRoot, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}

		rel, err := filepath.Rel(root, path)
		if err != nil {
			return err
		}

		if d.IsDir() {
			skip, err := PathMatchAny(rel, ignore...)
			if err != nil {
				return err
			}

			if skip {
				return filepath.SkipDir
			}
			// Only match files
			return nil
		}

		var match bool
		if alwaysMatch {
			match = true
		} else {
			match, err = doublestar.PathMatch(pattern, rel)
			if err != nil {
				return err
			}
		}

		if match {
			skip, err := PathMatchAny(rel, ignore...)
			if err != nil {
				return err
			}

			if skip {
				return nil
			}

			err = fn(rel, d, nil)
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
