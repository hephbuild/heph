package xfs

import (
	"context"
	"errors"
	"fmt"
	"github.com/bmatcuk/doublestar/v4"
	"github.com/hephbuild/heph/utils/ads"
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

func PathMatchExactOrPrefixAny(path string, matchers ...string) (bool, error) {
	for _, matcher := range matchers {
		if path == matcher || matchPrefix(path, matcher) {
			return true, nil
		}
	}

	return false, nil
}

func PathMatchAny(path string, matchers ...string) (bool, error) {
	path = filepath.Clean(path)

	for _, matcher := range matchers {
		matcher = filepath.Clean(matcher)

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

func StarWalk(ctx context.Context, root, pattern string, ignore []string, fn fs.WalkDirFunc) error {
	return starWalk(ctx, root, pattern, ignore, fn, false)
}

func StarWalkAbs(ctx context.Context, root, pattern string, ignore []string, fn fs.WalkDirFunc) error {
	return starWalk(ctx, root, pattern, ignore, fn, true)
}

func starWalk(ctx context.Context, root, pattern string, ignore []string, fn fs.WalkDirFunc, abs bool) error {
	if !filepath.IsAbs(root) {
		return fmt.Errorf("root must be abs")
	}

	if !filepath.IsAbs(pattern) {
		pattern = filepath.Join(root, pattern)
	}
	ignore = ads.Copy(ignore)
	for i := range ignore {
		if !filepath.IsAbs(ignore[i]) {
			ignore[i] = filepath.Join(root, ignore[i])
		}
	}

	alwaysMatch := false
	walkRoot := root

	i := indexMeta(pattern)
	if i == -1 {
		// Pattern is actually a pure path

		path := unescapeMeta(pattern)
		_, err := os.Lstat(path)
		if err != nil {
			if errors.Is(err, fs.ErrNotExist) {
				return nil
			}

			return err
		}

		// If dir: all files recursively would match
		// If file: it will call fn once with the file
		alwaysMatch = true
		walkRoot = path
	} else if i > 0 {
		i := strings.LastIndex(pattern[:i], string(filepath.Separator))
		if i > 0 {
			walkRoot = unescapeMeta(pattern[:i])
		}
	}

	return filepath.WalkDir(walkRoot, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}

		if err := ctx.Err(); err != nil {
			return err
		}

		if d.IsDir() {
			skip, err := PathMatchAny(path, ignore...)
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
			match, err = doublestar.PathMatch(pattern, path)
			if err != nil {
				return err
			}
		}

		if match {
			skip, err := PathMatchAny(path, ignore...)
			if err != nil {
				return err
			}

			if skip {
				return nil
			}

			if abs {
				err = fn(path, d, nil)
				if err != nil {
					return err
				}
			} else {
				rel, err := filepath.Rel(root, path)
				if err != nil {
					return err
				}

				err = fn(rel, d, nil)
				if err != nil {
					return err
				}
			}
		}

		return nil
	})
}
