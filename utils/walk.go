package utils

import (
	"github.com/bmatcuk/doublestar/v4"
	"io/fs"
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

func StarWalk(root, pattern string, ignore []string, fn fs.WalkDirFunc) error {
	walkRoot := root
	if i := strings.Index(pattern, "**/"); i > 0 {
		walkRoot = filepath.Join(root, pattern[:i])
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
