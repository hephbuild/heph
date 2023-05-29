package xfs

import (
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
)

func CpHardlink(from, to string) error {
	info, err := os.Lstat(from)
	if err != nil {
		return err
	}

	if !info.IsDir() {
		err := CreateParentDir(to)
		if err != nil {
			return err
		}

		err = os.RemoveAll(to)
		if err != nil {
			return err
		}

		err = os.Link(from, to)
		if err != nil {
			return Cp(from, to)
		}
		return nil
	}

	entries, err := os.ReadDir(from)
	if err != nil {
		return err
	}

	for _, entry := range entries {
		err := CpHardlink(filepath.Join(from, entry.Name()), filepath.Join(to, entry.Name()))
		if err != nil {
			return err
		}
	}

	return nil
}

func Cp(from, to string) error {
	logerr := func(id string, err error) error {
		return fmt.Errorf("%v %v to %v: %w", id, from, to, err)
	}

	info, err := os.Lstat(from)
	if err != nil {
		return logerr("cp1", err)
	}

	if info.IsDir() {
		return cpDir(from, to)
	}

	if !info.Mode().IsRegular() {
		link, err := os.Readlink(from)
		if err != nil {
			return logerr("cp11", err)
		}

		if filepath.IsAbs(link) {
			return fmt.Errorf("absolute link not allowed: %v -> %v", from, link)
		}

		err = CreateParentDir(to)
		if err != nil {
			return logerr("cp12", err)
		}

		err = os.Symlink(link, to)
		if err != nil {
			return logerr("cp13", err)
		}

		return nil
	}

	fromf, err := os.Open(from)
	if err != nil {
		return logerr("cp4", err)
	}
	defer fromf.Close()

	err = CreateParentDir(to)
	if err != nil {
		return logerr("cp5", err)
	}

	tof, err := os.OpenFile(to, os.O_RDWR|os.O_CREATE|os.O_TRUNC, os.ModePerm)
	if err != nil {
		return logerr("cp6", err)
	}
	defer tof.Close()

	_, err = io.Copy(tof, fromf)
	if err != nil {
		return logerr("cp7", err)
	}

	err = tof.Close()
	if err != nil {
		return err
	}

	err = os.Chmod(to, info.Mode())
	if err != nil {
		return err
	}

	err = os.Chtimes(to, info.ModTime(), info.ModTime())
	if err != nil {
		return logerr("cp8", err)
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
