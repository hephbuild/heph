package hfs

import (
	"errors"
	"fmt"
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
	path = fs.Path(path)
	if dir := filepath.Dir(path); dir != "." {
		err := fs.At(dir).MkdirAll("", ModePerm)
		if err != nil {
			return err
		}
	}

	return nil
}

func At[T FS](fs T, names ...string) T {
	for _, name := range names {
		fs = fs.At(name).(T) //nolint:errcheck
	}
	return fs
}

type WalkDirFunc = iofs.WalkDirFunc

type iofsAdapter struct {
	fs FS
}

func (i iofsAdapter) ReadDir(name string) ([]iofs.DirEntry, error) {
	return i.fs.ReadDir(name)
}

func (i iofsAdapter) Stat(name string) (iofs.FileInfo, error) {
	return i.fs.Stat(name)
}

func (i iofsAdapter) Open(name string) (iofs.File, error) {
	return Open(i.fs, name)
}

type StdFS interface {
	iofs.FS
	iofs.StatFS
	iofs.ReadDirFS
}

func ToIOFS(fs FS) StdFS {
	return iofsAdapter{fs: fs}
}

func Walk(fs FS, walkFn WalkDirFunc) error {
	return iofs.WalkDir(ToIOFS(fs), "", walkFn)
}

func Move(from, to FS) error {
	fromos, ok := from.(OS)
	if !ok {
		return fmt.Errorf("cannot move filesystem from %T to %T", from, to)
	}

	toos, ok := to.(OS)
	if !ok {
		return fmt.Errorf("cannot move filesystem from %T to %T", from, to)
	}

	return fromos.Move("", toos.Path())
}

func Copy(from, to FS) error {
	fromi, err := from.Stat("")
	if err != nil {
		return err
	}

	if fromi.IsDir() {
		return errors.New("unsupported codepath")
	}

	fromf, err := Open(from, "")
	if err != nil {
		return err
	}
	defer fromf.Close()

	// toi, err := to.Stat("")
	// if err != nil {
	//	return err
	//}

	tof, err := Create(to, "")
	if err != nil {
		return err
	}
	defer tof.Close()

	_, err = io.Copy(tof, fromf)
	if err != nil {
		return err
	}

	return nil
}
