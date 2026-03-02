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

type RONode interface {
	Stat() (FileInfo, error)
	Lstat() (FileInfo, error)
	Open(flag int, perm FileMode) (File, error)
	ReadDir() ([]DirEntry, error)
	Path() string
	AtRO(name string) RONode
}

type Node interface {
	RONode

	Move(to Node) error
	Remove() error
	RemoveAll() error
	Mkdir(mode FileMode) error
	MkdirAll(mode FileMode) error
	At(names ...string) Node
}
