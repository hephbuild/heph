package hfs

import (
	"errors"
	iofs "io/fs"
	"path"
)

// IOFS wraps an [io.FS] and implements [FS].
// Path semantics: hfs uses "" for the root, while io/fs uses ".".
type IOFS struct {
	fs   iofs.FS
	root string // relative to the underlying io.FS root, always clean
}

// Compile-time checks: IOFS satisfies both ROFS and the full FS interface.
var _ ROFS = (*IOFS)(nil)

// FromIOFS creates a new [IOFS] wrapping the given [io.FS].
func FromIOFS(fsys iofs.FS) *IOFS {
	return &IOFS{fs: fsys}
}

// fsPath converts an hfs path (where "" means root) to an io/fs path (where "." means root).
func (f *IOFS) fsPath(name string) string {
	if f.root == "" && name == "" {
		return "."
	}
	if f.root == "" {
		return name
	}
	if name == "" {
		return f.root
	}

	return path.Join(f.root, name)
}

func (f *IOFS) Stat(name string) (FileInfo, error) {
	p := f.fsPath(name)
	sfs, ok := f.fs.(iofs.StatFS)
	if ok {
		return sfs.Stat(p)
	}
	// Fallback: open the file and stat it.
	file, err := f.fs.Open(p)
	if err != nil {
		return nil, err
	}
	defer file.Close()

	return file.Stat()
}

// Lstat falls back to Stat because io.FS does not expose symlink information.
func (f *IOFS) Lstat(name string) (FileInfo, error) {
	return f.Stat(name)
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

func (f *IOFS) Open(name string, _ int, _ FileMode) (File, error) {
	p := f.fsPath(name)
	file, err := f.fs.Open(p)
	if err != nil {
		return nil, err
	}

	return iofsFile{File: file, name: p}, nil
}

func (f *IOFS) ReadDir(name string) ([]DirEntry, error) {
	p := f.fsPath(name)
	rdfs, ok := f.fs.(iofs.ReadDirFS)
	if ok {
		return rdfs.ReadDir(p)
	}

	// Fallback via iofs.ReadDir helper.
	return iofs.ReadDir(f.fs, p)
}

// AtRO returns a new ROFS rooted at name within the underlying io.FS.
func (f *IOFS) AtRO(name string) ROFS {
	newRoot := path.Join(f.root, name)

	return &IOFS{fs: f.fs, root: newRoot}
}

func (f *IOFS) Path(names ...string) string {
	parts := make([]string, 0, len(names)+1)
	if f.root != "" {
		parts = append(parts, f.root)
	}
	parts = append(parts, names...)

	return path.Join(parts...)
}
