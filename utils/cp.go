package utils

import (
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
)

func Cp(from, to string) error {
	fromf, err := os.Open(from)
	if err != nil {
		return fmt.Errorf("cp1: %w", err)
	}
	defer fromf.Close()

	info, err := fromf.Stat()
	if err != nil {
		return fmt.Errorf("cp3: %w", err)
	}

	if info.IsDir() {
		return cpDir(from, to)
	}

	if dir := filepath.Dir(to); dir != "." {
		err = os.MkdirAll(dir, os.ModePerm)
		if err != nil {
			return fmt.Errorf("cp2: %w", err)
		}
	}

	tof, err := os.OpenFile(to, os.O_RDWR|os.O_CREATE|os.O_TRUNC, info.Mode().Perm())
	if err != nil {
		return fmt.Errorf("cp4: %w", err)
	}
	defer tof.Close()

	err = os.Chtimes(to, info.ModTime(), info.ModTime())
	if err != nil {
		return fmt.Errorf("cp5: %w", err)
	}

	_, err = io.Copy(tof, fromf)
	if err != nil {
		return fmt.Errorf("cp6: %w", err)
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
