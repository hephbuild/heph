package hfs

import (
	"github.com/hephbuild/hephv2/internal/flock"
	"io/fs"
	"os"
	"path/filepath"
	"time"
)

func NewOS(root string) OS {
	return OS{root: root}
}

func AsOs(fs FS) (OS, bool) {
	osfs, ok := fs.(OS)

	return osfs, ok
}

type OS struct {
	root string
}

var _ FS = (*OS)(nil)
var _ FileReader = (*OS)(nil)

func (osfs OS) join(name string) string {
	if name == "" {
		return osfs.root
	}

	if filepath.IsAbs(name) {
		return name
	}

	if osfs.root == "" {
		return name
	}

	return filepath.Join(osfs.root, name)
}

func (osfs OS) At(name string) FS {
	osfs.root = osfs.join(name)

	return osfs
}

func (osfs OS) Stat(name string) (FileInfo, error) {
	return os.Stat(osfs.join(name))
}

func (osfs OS) Lstat(name string) (FileInfo, error) {
	return os.Lstat(osfs.join(name))
}

func (osfs OS) Open(name string, flag int, perm FileMode) (File, error) {
	return os.OpenFile(osfs.join(name), flag, perm)
}

func (osfs OS) Move(oldname, newname string) error {
	return os.Rename(osfs.join(oldname), osfs.join(newname))
}

func (osfs OS) Remove(path string) error {
	return os.Remove(osfs.join(path))
}

func (osfs OS) RemoveAll(path string) error {
	return os.RemoveAll(osfs.join(path))
}

func (osfs OS) Chown(name string, uid, gid int) error {
	return os.Chown(osfs.join(name), uid, gid)
}

func (osfs OS) Chmod(name string, mode os.FileMode) error {
	return os.Chmod(osfs.join(name), mode)
}

func (osfs OS) Chtimes(name string, atime time.Time, mtime time.Time) error {
	return os.Chtimes(osfs.join(name), atime, mtime)
}

func (osfs OS) Symlink(oldname, newname string) error {
	return os.Symlink(oldname, osfs.join(newname))
}

func (osfs OS) Mkdir(name string, mode os.FileMode) error {
	return os.Mkdir(osfs.join(name), mode)
}

func (osfs OS) MkdirAll(name string, mode os.FileMode) error {
	return os.MkdirAll(osfs.join(name), mode)
}

// Inspired from https://github.com/golang/go/blob/3c72dd513c30df60c0624360e98a77c4ae7ca7c8/src/cmd/go/internal/modfetch/fetch.go

func (osfs OS) MakeDirsReadOnly(dir string) {
	type pathMode struct {
		path string
		mode fs.FileMode
	}
	var dirs []pathMode // in lexical order
	_ = filepath.WalkDir(osfs.join(dir), func(path string, d fs.DirEntry, err error) error {
		if err == nil && d.IsDir() {
			info, err := d.Info()
			if err == nil && info.Mode()&0222 != 0 {
				dirs = append(dirs, pathMode{path, info.Mode()})
			}
		}
		return nil
	})

	// Run over list backward to chmod children before parents.
	for i := len(dirs) - 1; i >= 0; i-- {
		_ = os.Chmod(dirs[i].path, dirs[i].mode&^0222)
	}
}

func (osfs OS) MakeDirsReadWrite(dir string) {
	// Module cache has 0555 directories; make them writable in order to remove content.
	_ = filepath.WalkDir(osfs.join(dir), func(path string, info fs.DirEntry, err error) error {
		if err != nil {
			return nil // ignore errors walking in file system
		}
		if info.IsDir() {
			_ = os.Chmod(path, 0777)
		}
		return nil
	})
}

func (osfs OS) ReadFile(filename string) ([]byte, error) {
	return os.ReadFile(osfs.join(filename))
}

func (osfs OS) ReadDir(name string) ([]DirEntry, error) {
	return os.ReadDir(osfs.join(name))
}

func (osfs OS) Path(elems ...string) string {
	args := []string{osfs.root}
	args = append(args, elems...)

	return filepath.Join(args...)
}

func (osfs OS) CloseEnsureROFD(hf File) error {
	f := hf.(*os.File)

	err := flock.Flock(f, false, true)
	if err != nil {
		return err
	}

	err = f.Close()
	if err != nil {
		return err
	}

	f, err = os.OpenFile(f.Name(), os.O_RDONLY, 0)
	if err != nil {
		return err
	}
	defer f.Close()

	err = flock.Flock(f, false, true)
	if err != nil {
		return err
	}

	err = f.Close()
	if err != nil {
		return err
	}

	return nil
}
