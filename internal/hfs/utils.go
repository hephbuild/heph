package hfs

import (
	"errors"
	"fmt"
	"io"
	iofs "io/fs"
	"os"
	"path/filepath"
)

func Exists(node RONode) bool {
	_, err := node.Lstat()
	return err == nil
}

func Open(node RONode) (File, error) {
	return node.Open(os.O_RDONLY, 0)
}

type FileReader interface {
	ReadFile() ([]byte, error)
}

func ReadFile(node RONode) ([]byte, error) {
	if fr, ok := node.(FileReader); ok {
		return fr.ReadFile()
	}

	f, err := Open(node)
	if err != nil {
		return nil, err
	}
	defer f.Close()

	return io.ReadAll(f)
}

func WriteFile(node Node, b []byte) error {
	f, err := Create(node)
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

func Create(node Node) (File, error) {
	err := CreateParentDir(node)
	if err != nil {
		return nil, err
	}

	return node.Open(os.O_RDWR|os.O_CREATE|os.O_TRUNC, 0666)
}

func CreateExec(node Node) (File, error) {
	err := CreateParentDir(node)
	if err != nil {
		return nil, err
	}

	return node.Open(os.O_RDWR|os.O_CREATE|os.O_TRUNC, 0666|0111)
}

func CreateParentDir(node Node) error {
	p := node.Path()
	if dir := filepath.Dir(p); dir != "." && dir != p {
		err := At(node, dir).MkdirAll(ModePerm)
		if err != nil {
			return err
		}
	}

	return nil
}

func At[T any](node T, names ...string) T {
	for _, name := range names {
		switch n := any(node).(type) {
		case Node:
			node = n.At(name).(T) //nolint:errcheck
		case RONode:
			node = n.AtRO(name).(T) //nolint:errcheck
		default:
			panic(fmt.Sprintf("At: unknown node type: %T", n))
		}
	}
	return node
}

type WalkDirFunc = iofs.WalkDirFunc

func Walk(node Node, walkFn WalkDirFunc) error {
	return iofs.WalkDir(ToIOFS(node), ".", walkFn)
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
