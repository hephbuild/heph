package hfs

import (
	"io"
	"io/fs"
	"os"
)

type FileInfo = fs.FileInfo
type FileMode = fs.FileMode
type DirEntry = fs.DirEntry

const ModePerm = fs.ModePerm
const ModeDir = fs.ModeDir
const ModeDefault = 0770

var ErrNotExist = fs.ErrNotExist

type File interface {
	io.ReadWriteCloser
	Stat() (FileInfo, error)
	LStat() (FileInfo, error)
	Name() string
}

type OSFile interface {
	GetOSFile() *os.File
}

type ROFS interface {
	Stat(name string) (FileInfo, error)
	Lstat(name string) (FileInfo, error)
	Open(name string, flag int, perm FileMode) (File, error)
	ReadDir(name string) ([]DirEntry, error)
	Path(names ...string) string
	AtRO(name string) ROFS
}

type FS interface {
	ROFS

	Move(oldname, newname string) error
	Remove(path string) error
	RemoveAll(path string) error
	Mkdir(name string, mode FileMode) error
	MkdirAll(name string, mode FileMode) error
	At(name string) FS
}
