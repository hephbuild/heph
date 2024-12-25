package hfs

import (
	"io"
	"io/fs"
)

type FileInfo = fs.FileInfo
type FileMode = fs.FileMode

const ModePerm = fs.ModePerm
const ModeDefault = 0770

var ErrNotExist = fs.ErrNotExist

type File interface {
	io.ReadWriteCloser
	Stat() (FileInfo, error)
	Name() string
}

type FS interface {
	Stat(name string) (FileInfo, error)
	Lstat(name string) (FileInfo, error)
	Open(name string, flag int, perm FileMode) (File, error)
	Move(oldname, newname string) error
	Remove(path string) error
	RemoveAll(path string) error
	Mkdir(name string, mode FileMode) error
	MkdirAll(name string, mode FileMode) error
	At(name string) FS
}
