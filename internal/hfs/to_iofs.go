package hfs

import iofs "io/fs"

type toIofsAdapter struct {
	fs RONode
}

func (i toIofsAdapter) ReadDir(name string) ([]iofs.DirEntry, error) {
	return i.fs.AtRO(name).ReadDir()
}

func (i toIofsAdapter) Stat(name string) (iofs.FileInfo, error) {
	return i.fs.AtRO(name).Stat()
}

func (i toIofsAdapter) Open(name string) (iofs.File, error) {
	return Open(i.fs.AtRO(name))
}

type StdFS interface {
	iofs.FS
	iofs.StatFS
	iofs.ReadDirFS
}

func ToIOFS(fs RONode) StdFS {
	return toIofsAdapter{fs: fs}
}
