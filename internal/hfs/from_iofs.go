package hfs

import (
	"errors"
	iofs "io/fs"
	"path"
)

// fromIofsAdapter wraps an [io.FS] and implements [RONode].
// Path semantics: hfs uses "" for the root, while io/fs uses ".".
type fromIofsAdapter struct {
	fs   iofs.FS
	path string // relative to the underlying io.FS path, always clean
}

// Compile-time checks: fromIofsAdapter satisfies RONode.
var _ RONode = (*fromIofsAdapter)(nil)

// FromIOFS creates a new [fromIofsAdapter] wrapping the given [io.FS].
func FromIOFS(fsys iofs.FS) RONode {
	return &fromIofsAdapter{fs: fsys}
}

func (f *fromIofsAdapter) Stat() (FileInfo, error) {
	sfs, ok := f.fs.(iofs.StatFS)
	if ok {
		return sfs.Stat(f.path)
	}
	// Fallback: open the file and stat it.
	file, err := f.fs.Open(f.path)
	if err != nil {
		return nil, err
	}
	defer file.Close()

	return file.Stat()
}

// Lstat falls back to Stat because io.FS does not expose symlink information.
func (f *fromIofsAdapter) Lstat() (FileInfo, error) {
	return f.Stat()
}

// iofsFile wraps an [iofs.File] to satisfy [File].
// Write is not supported.
type iofsFile struct {
	iofs.File
	name string
}

func (iofsFile) Write(_ []byte) (int, error) {
	return 0, errors.New("hfs.IOFS: write not supported on read-only io.FS")
}

func (f iofsFile) LStat() (FileInfo, error) {
	return f.Stat()
}

func (f iofsFile) Name() string {
	return f.name
}

func (f *fromIofsAdapter) Open(_ int, _ FileMode) (File, error) {
	p := f.path

	file, err := f.fs.Open(p)
	if err != nil {
		return nil, err
	}

	return iofsFile{File: file, name: p}, nil
}

func (f *fromIofsAdapter) ReadDir() ([]DirEntry, error) {
	p := f.path

	rdfs, ok := f.fs.(iofs.ReadDirFS)
	if ok {
		return rdfs.ReadDir(p)
	}

	// Fallback via iofs.ReadDir helper.
	return iofs.ReadDir(f.fs, p)
}

// AtRO returns a new RONode rooted at name within the underlying io.FS.
func (f *fromIofsAdapter) AtRO(name string) RONode {
	newRoot := path.Join(f.path, name)

	return &fromIofsAdapter{fs: f.fs, path: newRoot}
}

func (f *fromIofsAdapter) Path() string {
	if f.path == "" {
		return ""
	}
	return f.path
}
