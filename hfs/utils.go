package hfs

import (
	"io"
	iofs "io/fs"
	"os"
	"path/filepath"
)

func Exists(fs FS, filename string) bool {
	_, err := fs.Lstat(filename)
	return err == nil
}

func Open(fs FS, filename string) (File, error) {
	return fs.Open(filename, os.O_RDONLY, 0)
}

type FileReader interface {
	ReadFile(filename string) ([]byte, error)
}

func ReadFile(fs FS, filename string) ([]byte, error) {
	if fr, ok := fs.(FileReader); ok {
		return fr.ReadFile(filename)
	}

	f, err := Open(fs, filename)
	if err != nil {
		return nil, err
	}
	defer f.Close()

	return io.ReadAll(f)
}

func WriteFile(fs FS, filename string, b []byte, mode FileMode) error {
	f, err := Create(fs, filename)
	if err != nil {
		return err
	}

	_, err = f.Write(b)
	if err != nil {
		return err
	}

	return f.Close()
}

func Create(fs FS, filename string) (File, error) {
	err := CreateParentDir(fs, filename)
	if err != nil {
		return nil, err
	}

	return fs.Open(filename, os.O_RDWR|os.O_CREATE|os.O_TRUNC, 0666)
}

func CreateParentDir(fs FS, path string) error {
	if dir := filepath.Dir(path); dir != "." {
		err := fs.MkdirAll(dir, ModePerm)
		if err != nil {
			return err
		}
	}

	return nil
}

func At[T FS](fs T, names ...string) T {
	for _, name := range names {
		fs = fs.At(name).(T)
	}
	return fs
}

type WalkDirFunc = iofs.WalkDirFunc

type iofsAdapter struct {
	fs FS
}

func (i iofsAdapter) Open(name string) (iofs.File, error) {
	return Open(i.fs, name)
}

func ToIOFS(fs FS) iofs.FS {
	return iofsAdapter{fs: fs}
}

func Walk(fs FS, walkFn WalkDirFunc) error {
	return iofs.WalkDir(ToIOFS(fs), "", walkFn)
}
