package lib

import (
	"bytes"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
	"time"
)

func FileContent(p string) (string, error) {
	b, err := os.ReadFile(p)
	if err != nil {
		return "", err
	}

	return strings.TrimSpace(string(b)), nil
}

func FileModTime(p string) (string, error) {
	i, err := os.Stat(p)
	if err != nil {
		return "", err
	}

	return i.ModTime().Format(time.RFC3339Nano), nil
}

func WriteFile(p, content string) error {
	return os.WriteFile(p, []byte(content), os.ModePerm)
}

func ReWriteFile(p, content string) error {
	_, err := FileModTime(p)
	if err != nil {
		return err
	}

	return os.WriteFile(p, []byte(content), os.ModePerm)
}

func ReplaceFile(p, old, new string) error {
	b, err := os.ReadFile(p)
	if err != nil {
		return err
	}

	nb := bytes.ReplaceAll(b, []byte(old), []byte(new))

	return os.WriteFile(p, nb, os.ModePerm)
}

// From https://github.com/golang/go/blob/3c72dd513c30df60c0624360e98a77c4ae7ca7c8/src/cmd/go/internal/modfetch/fetch.go

func RemoveAll(dir string) error {
	// Module cache has 0555 directories; make them writable in order to remove content.
	filepath.WalkDir(dir, func(path string, info fs.DirEntry, err error) error {
		if err != nil {
			return nil // ignore errors walking in file system
		}
		if info.IsDir() {
			os.Chmod(path, 0777)
		}
		return nil
	})
	return os.RemoveAll(dir)
}
