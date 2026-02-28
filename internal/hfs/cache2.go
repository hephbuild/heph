package hfs

import (
	"errors"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"strings"

	sync_map "github.com/zolstein/sync-map"
)

// TODO: move to context or RequestState
var DefaultOSCache = NewFSCache()

// node2 represents a file or directory node in the FSCache tree.
type node2 struct {
	path     string
	info     fs.FileInfo
	scanned  bool // true if directory children have been read from disk
	children []*node2
}

// FSCache implements fs.FS using absolute OS paths. Unlike the standard
// io/fs contract (which requires relative paths), Open accepts any absolute
// path, which means you can pass FSCache directly to fs.WalkDir with an
// absolute root:
//
//	cache := hfs.NewFSCache2()
//	fs.WalkDir(cache, "/some/dir", fn)
//
// Directory listings are read from disk once and cached; subsequent walks of
// the same subtree are served entirely from memory.
type FSCache struct {
	nodes sync_map.Map[string, *node2]
}

// NewFSCache creates an empty FSCache. It has no associated root; any
// absolute path on the machine can be opened through it.
func NewFSCache() *FSCache {
	return &FSCache{}
}

// Open implements fs.FS. name is treated as an absolute OS path (or "." which
// is a no-op placeholder). The first time a directory is opened its children
// are read from disk and cached for all future calls.
func (c *FSCache) Open(name string) (fs.File, error) {
	abs := filepath.Clean(name)

	n, err := c.getOrLoad(abs)
	if err != nil {
		return nil, &fs.PathError{Op: "open", Path: name, Err: err}
	}

	return &cachedFile2{cache: c, node: n, name: name}, nil
}

// getOrLoad returns the node for abs, loading from disk when absent.
func (c *FSCache) getOrLoad(abs string) (*node2, error) {
	if n, ok := c.nodes.Load(abs); ok {
		return n, nil
	}

	info, err := os.Lstat(abs)
	if err != nil {
		return nil, err
	}

	if info.Mode().Type()&os.ModeSymlink != 0 {
		info, err = os.Stat(abs)
		if err != nil {
			return nil, err
		}
	}

	n := &node2{path: abs, info: info}
	c.nodes.Store(abs, n)
	return n, nil
}

// ensureScanned populates n.children from disk if not already done.
func (c *FSCache) ensureScanned(n *node2) error {
	if n.scanned {
		return nil
	}

	entries, err := os.ReadDir(n.path)
	if err != nil {
		return err
	}

	children := make([]*node2, 0, len(entries))
	for _, e := range entries {
		childAbs := filepath.Join(n.path, e.Name())

		child, ok := c.nodes.Load(childAbs)
		if !ok {
			info, err := e.Info()
			if err != nil {
				continue
			}
			// Follow symlinks so that a symlink pointing to a directory
			// is correctly treated as a directory when walking.
			if info.Mode().Type()&os.ModeSymlink != 0 {
				resolved, err := os.Stat(childAbs)
				if err == nil {
					info = resolved
				}
			}
			child = &node2{path: childAbs, info: info}
			c.nodes.Store(childAbs, child)
		}

		children = append(children, child)
	}

	// Sort for deterministic order (mirrors fs.WalkDir behaviour).
	slices.SortFunc(children, func(a, b *node2) int {
		return strings.Compare(a.info.Name(), b.info.Name())
	})

	n.children = children // set after slice is immutable for safe concurrent reads
	n.scanned = true
	return nil
}

// ---- cachedFile2 -----------------------------------------------------------

// cachedFile2 implements fs.File (and fs.ReadDirFile for directories).
type cachedFile2 struct {
	cache  *FSCache
	node   *node2
	name   string // original name passed to Open, kept for error messages
	offset int    // directory read cursor

	// Opened lazily on first Read.
	osFile *os.File
}

func (f *cachedFile2) Stat() (fs.FileInfo, error) {
	return f.node.info, nil
}

func (f *cachedFile2) Read(b []byte) (int, error) {
	if f.node.info.IsDir() {
		return 0, &fs.PathError{Op: "read", Path: f.name, Err: errors.New("is a directory")}
	}
	if err := f.openOSFile(); err != nil {
		return 0, err
	}
	return f.osFile.Read(b)
}

func (f *cachedFile2) Close() error {
	if f.osFile != nil {
		return f.osFile.Close()
	}
	return nil
}

// ReadDir implements fs.ReadDirFile. fs.WalkDir calls this to enumerate
// directory children without re-opening each entry individually.
func (f *cachedFile2) ReadDir(n int) ([]fs.DirEntry, error) {
	if !f.node.info.IsDir() {
		return nil, &fs.PathError{Op: "readdir", Path: f.name, Err: errors.New("not a directory")}
	}

	if err := f.cache.ensureScanned(f.node); err != nil {
		return nil, &fs.PathError{Op: "readdir", Path: f.name, Err: err}
	}

	children := f.node.children
	remaining := children[f.offset:]

	if n <= 0 {
		result := make([]fs.DirEntry, len(remaining))
		for i, c := range remaining {
			result[i] = &cachedDirEntry2{info: c.info}
		}
		f.offset = len(children)
		return result, nil
	}

	if len(remaining) == 0 {
		return nil, io.EOF
	}

	count := n
	if count > len(remaining) {
		count = len(remaining)
	}

	result := make([]fs.DirEntry, count)
	for i, c := range remaining[:count] {
		result[i] = &cachedDirEntry2{info: c.info}
	}
	f.offset += count

	if count < len(remaining) {
		return result, nil
	}
	return result, io.EOF
}

func (f *cachedFile2) openOSFile() error {
	if f.osFile != nil {
		return nil
	}
	osf, err := os.Open(f.node.path)
	if err != nil {
		return &fs.PathError{Op: "open", Path: f.name, Err: err}
	}
	f.osFile = osf
	return nil
}

// ---- cachedDirEntry2 -------------------------------------------------------

type cachedDirEntry2 struct {
	info fs.FileInfo
}

func (e *cachedDirEntry2) Name() string               { return e.info.Name() }
func (e *cachedDirEntry2) IsDir() bool                { return e.info.IsDir() }
func (e *cachedDirEntry2) Type() fs.FileMode          { return e.info.Mode().Type() }
func (e *cachedDirEntry2) Info() (fs.FileInfo, error) { return e.info, nil }

// Compile-time interface checks.
var _ fs.FS = (*FSCache)(nil)
var _ fs.ReadDirFile = (*cachedFile2)(nil)
