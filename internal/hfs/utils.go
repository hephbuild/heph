package hfs

import (
	"errors"
	"fmt"
	"io"
	iofs "io/fs"
	"os"
	"path/filepath"
)

func Exists(fs RONode) bool {
	_, err := fs.Lstat()
	return err == nil
}

func Open(fs RONode) (File, error) {
	return fs.Open(os.O_RDONLY, 0)
}

type FileReader interface {
	ReadFile() ([]byte, error)
}

func ReadFile(fs RONode) ([]byte, error) {
	if fr, ok := fs.(FileReader); ok {
		return fr.ReadFile()
	}

	f, err := Open(fs)
	if err != nil {
		return nil, err
	}
	defer f.Close()

	return io.ReadAll(f)
}

func WriteFile(fs Node, b []byte) error {
	f, err := Create(fs)
	if err != nil {
		return err
	}
	defer f.Close()

	_, err = f.Write(b)
	if err != nil {
		return err
	}

	return f.Close()
}

func Create(fs Node) (File, error) {
	err := CreateParentDir(fs)
	if err != nil {
		return nil, err
	}

	return fs.Open(os.O_RDWR|os.O_CREATE|os.O_TRUNC, 0666)
}

func CreateExec(fs Node) (File, error) {
	err := CreateParentDir(fs)
	if err != nil {
		return nil, err
	}

	return fs.Open(os.O_RDWR|os.O_CREATE|os.O_TRUNC, 0666|0111)
}

func CreateParentDir(fs Node) error {
	p := fs.Path()
	if dir := filepath.Dir(p); dir != "." && dir != p {
		err := At(fs, dir).MkdirAll(ModePerm)
		if err != nil {
			return err
		}
	}

	return nil
}

func At[T any](fs T, names ...string) T {
	for _, name := range names {
		switch ffs := any(fs).(type) {
		case Node:
			fs = ffs.At(name).(T) //nolint:errcheck
		case RONode:
			fs = ffs.AtRO(name).(T) //nolint:errcheck
		default:
			panic(fmt.Sprintf("At: unknown FS type: %T", ffs))
		}
	}
	return fs
}

type WalkDirFunc = iofs.WalkDirFunc

func Walk(fs Node, walkFn WalkDirFunc) error {
	return iofs.WalkDir(ToIOFS(fs), ".", walkFn)
}

func Move(from, to Node) error {
	err := CreateParentDir(to)
	if err != nil {
		return err
	}

	return from.Move(to)
}

func Copy(from, to Node) error {
	fromi, err := from.Stat()
	if err != nil {
		return err
	}

	if fromi.IsDir() {
		return errors.New("unsupported codepath")
	}

	fromf, err := Open(from)
	if err != nil {
		return err
	}
	defer fromf.Close()

	tof, err := Create(to)
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
