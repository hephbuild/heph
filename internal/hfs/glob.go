package hfs

import (
	"context"
	"errors"
	iofs "io/fs"
	"path/filepath"
	"strings"

	"github.com/bmatcuk/doublestar/v4"
)

// Equivalent to `strings.HasPrefix(path, prefix+"/")`, without the string concat.
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

// Unescapes meta characters (*?[]{}).
func unescapeMeta(pattern string) string {
	return metaReplacer.Replace(pattern)
}

func indexMeta(s string) int {
	var c byte
	l := len(s)
	for i := 0; i < l; i++ {
		c = s[i]
		switch c {
		case '*', '?', '[', '{':
			return i
		case '\\':
			// skip next byte
			i++
		}
	}
	return -1
}

func IsGlob(path string) bool {
	return indexMeta(path) != -1
}

type GlobWalkFunc func(path string, d DirEntry) error

func Glob(ctx context.Context, fs FS, pattern string, ignore []string, fn GlobWalkFunc) error {
	return glob(ctx, fs, pattern, ignore, fn)
}

func glob(ctx context.Context, fs FS, pattern string, ignore []string, fn GlobWalkFunc) error {
	prefix := ""

	i := indexMeta(pattern)
	if i == -1 {
		info, err := fs.Lstat(unescapeMeta(pattern))
		if err != nil {
			if errors.Is(err, ErrNotExist) {
				return nil
			}

			return err
		}

		if info.IsDir() {
			prefix = pattern
			pattern = "**/*"
		}
	} else {
		prefix = unescapeMeta(pattern[:i])
		pattern = pattern[i:]
	}

	walkfs := fs
	if prefix != "" {
		walkfs = At(fs, prefix)
	}

	if pattern == "**/*" {
		return globAll(ctx, walkfs, prefix, ignore, fn)
	}

	return doublestar.GlobWalk(ToIOFS(walkfs), pattern, func(path string, d iofs.DirEntry) error {
		return innerGlob(ctx, filepath.Join(prefix, path), ignore, d, fn)
	})
}

func globAll(ctx context.Context, fs FS, prefix string, ignore []string, fn GlobWalkFunc) error {
	return iofs.WalkDir(ToIOFS(fs), "", func(path string, d DirEntry, err error) error {
		if err != nil {
			return err
		}

		return innerGlob(ctx, filepath.Join(prefix, path), ignore, d, fn)
	})
}

func innerGlob(ctx context.Context, path string, ignore []string, d iofs.DirEntry, fn GlobWalkFunc) error {
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

	skip, err := PathMatchAny(path, ignore...)
	if err != nil {
		return err
	}

	if skip {
		return nil
	}

	err = fn(path, d)
	if err != nil {
		return err
	}

	return nil
}
