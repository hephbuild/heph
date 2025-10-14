package hfs

import (
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/hephbuild/heph/internal/flock"
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

type wrapOsInfo struct {
	os.FileInfo
	name string
}

// os.FileInfo.Name() returns the base name of the file, while os.File.Name() returns the full path.
func (i wrapOsInfo) Name() string {
	return i.name
}

func (osfs OS) Lstat(name string) (FileInfo, error) {
	path := osfs.join(name)
	info, err := os.Lstat(path)
	if err != nil {
		return nil, err
	}

	return wrapOsInfo{FileInfo: info, name: path}, err
}

func (osfs OS) Open(name string, flag int, perm FileMode) (File, error) {
	return os.OpenFile(osfs.join(name), flag, perm)
}

func (osfs OS) Move(oldname, newname string) error {
	oldpath := osfs.join(oldname)
	newpath := osfs.join(newname)

	err := os.Rename(oldpath, newpath)
	if err != nil {
		if strings.Contains(err.Error(), "invalid cross-device link") {
			return osfs.moveCrossDevice(oldpath, newpath)
		}

		return err
	}

	return nil
}

func (osfs OS) moveCrossDevice(source, destination string) error {
	src, err := os.Open(source)
	if err != nil {
		return fmt.Errorf("open: %w", err)
	}

	dst, err := os.Create(destination)
	if err != nil {
		_ = src.Close()
		return fmt.Errorf("create: %w", err)
	}
	_, err = io.Copy(dst, src)
	_ = src.Close()
	_ = dst.Close()
	if err != nil {
		return fmt.Errorf("copy: %w", err)
	}

	fi, err := os.Stat(source)
	if err != nil {
		_ = os.Remove(destination)
		return fmt.Errorf("stat: %w", err)
	}

	err = os.Chmod(destination, fi.Mode())
	if err != nil {
		_ = os.Remove(destination)
		return fmt.Errorf("stat: %w", err)
	}
	_ = os.Remove(source)

	return nil
}

func (osfs OS) Remove(path string) error {
	return os.Remove(osfs.join(path))
}

func (osfs OS) RemoveAll(path string) error {
	dir := osfs.join(path)

	MakeDirsReadWrite(dir)

	return os.RemoveAll(dir)
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
			return nil //nolint:nilerr // ignore errors walking in file system
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
	f, ok := hf.(*os.File)
	if !ok {
		return hf.Close()
	}

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

// Inspired from https://github.com/golang/go/blob/3c72dd513c30df60c0624360e98a77c4ae7ca7c8/src/cmd/go/internal/modfetch/fetch.go

func MakeDirsReadOnly(dir string) {
	type pathMode struct {
		path string
		mode fs.FileMode
	}
	var dirs []pathMode // in lexical order
	_ = filepath.WalkDir(dir, func(path string, d fs.DirEntry, err error) error {
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

func MakeDirsReadWrite(dir string) {
	// Module cache has 0555 directories; make them writable in order to remove content.
	_ = filepath.WalkDir(dir, func(path string, info fs.DirEntry, err error) error {
		if err != nil {
			return nil //nolint:nilerr // ignore errors walking in file system
		}
		if info.IsDir() {
			_ = os.Chmod(path, 0777)
		}
		return nil
	})
}
