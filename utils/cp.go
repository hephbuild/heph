package utils

import (
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
)

func Cp(from, to string) error {
	logerr := func(id string, err error) error {
		return fmt.Errorf("%v %v to %v: %w", id, from, to, err)
	}

	fromf, err := os.Open(from)
	if err != nil {
		return logerr("cp1", err)
	}
	defer fromf.Close()

	info, err := fromf.Stat()
	if err != nil {
		return logerr("cp3", err)
	}

	if info.IsDir() {
		return cpDir(from, to)
	}

	if dir := filepath.Dir(to); dir != "." {
		err = os.MkdirAll(dir, os.ModePerm)
		if err != nil {
			return logerr("cp2", err)
		}
	}

	tof, err := os.OpenFile(to, os.O_RDWR|os.O_CREATE|os.O_TRUNC, info.Mode().Perm())
	if err != nil {
		return logerr("cp4", err)
	}
	defer tof.Close()

	_, err = io.Copy(tof, fromf)
	if err != nil {
		return logerr("cp5", err)
	}

	err = tof.Close()
	if err != nil {
		return err
	}

	err = os.Chtimes(to, info.ModTime(), info.ModTime())
	if err != nil {
		return logerr("cp6", err)
	}

	return nil
}

func cpDir(from, to string) error {
	return filepath.WalkDir(from, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}

		if d.IsDir() {
			return nil
		}

		rel, err := filepath.Rel(from, path)
		if err != nil {
			return err
		}

		return Cp(path, filepath.Join(to, rel))
	})
}
