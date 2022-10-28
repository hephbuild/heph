package fs

import (
	"errors"
	"heph/utils"
	"io"
	"os"
	"path/filepath"
)

func RandPath(base, prefix, suffix string) string {
	return filepath.Join(base, prefix+utils.RandStr(16)+suffix)
}

func IsDirEmpty(name string) (bool, error) {
	f, err := os.Open(name)
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return true, nil
		}

		return false, err
	}
	defer f.Close()

	// read in ONLY one file
	_, err = f.Readdir(1)

	// and if the file is EOF... well, the dir is empty.
	if err == io.EOF {
		return true, nil
	}
	return false, err
}

func PathExists(filename string) bool {
	_, err := os.Lstat(filename)
	return err == nil
}

func CreateParentDir(path string) error {
	if dir := filepath.Dir(path); dir != "." {
		err := os.MkdirAll(dir, os.ModePerm)
		if err != nil {
			return err
		}
	}

	return nil
}

func WriteFileSync(name string, data []byte, perm os.FileMode) error {
	f, err := os.OpenFile(name, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, perm)
	if err != nil {
		return err
	}
	_, err = f.Write(data)
	if err1 := f.Sync(); err1 != nil && err == nil {
		err = err1
	}
	if err1 := f.Close(); err1 != nil && err == nil {
		err = err1
	}
	return err
}
