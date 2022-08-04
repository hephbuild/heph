package utils

import (
	"github.com/bmatcuk/doublestar/v4"
	"io/fs"
	"path/filepath"
	"strings"
)

func isIgnored(s string, ignored []string) bool {
	parts := strings.Split(s, string(filepath.Separator))

	for _, p := range parts {
		for _, i := range ignored {
			if i == p {
				return true
			}
		}
	}

	return false
}

func StarWalk(root, pattern string, ignore []string, fn fs.WalkDirFunc) error {
	return filepath.WalkDir(root, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}

		if isIgnored(path, ignore) {
			if d.IsDir() {
				return filepath.SkipDir
			} else {
				return nil
			}
		}

		rel, err := filepath.Rel(root, path)
		if err != nil {
			return err
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
