package hfs

import (
	"bytes"
	"io"
	"io/fs"
	"os"
	"path"
	"strings"
	"time"
)

type memFile struct {
	b    []byte
	r    io.Reader
	info memFileInfo
}

func (m *memFile) Read(p []byte) (n int, err error) {
	if m.r == nil {
		m.r = bytes.NewReader(m.b)
	}

	return m.r.Read(p)
}

func (m *memFile) Write(p []byte) (n int, err error) {
	m.b = append(m.b, p...)
	m.info.size += int64(len(p))

	return len(p), nil
}

func (m *memFile) Close() error {
	m.r = nil

	return nil
}

func (m *memFile) Stat() (FileInfo, error) {
	return m.info, nil
}

func (m *memFile) Name() string {
	return m.info.Name()
}

type memFileInfo struct {
	name    string
	size    int64
	mode    fs.FileMode
	modtime time.Time
	isdir   bool
}

func (m memFileInfo) Name() string {
	return m.name
}

func (m memFileInfo) Size() int64 {
	return m.size
}

func (m memFileInfo) Mode() fs.FileMode {
	return m.mode
}

func (m memFileInfo) ModTime() time.Time {
	return m.modtime
}

func (m memFileInfo) IsDir() bool {
	return m.isdir
}

func (m memFileInfo) Sys() any {
	return nil
}

type Mem struct {
	prefix string
	m      map[string]*memFile
}

func NewMem() Mem {
	return Mem{
		m: make(map[string]*memFile),
	}
}

func (m Mem) join(name string) string {
	return path.Join(m.prefix, name)
}

func (m Mem) Stat(name string) (FileInfo, error) {
	f, ok := m.m[m.join(name)]
	if !ok {
		return nil, ErrNotExist
	}

	return f.info, nil
}

func (m Mem) Lstat(name string) (FileInfo, error) {
	return m.Stat(name)
}

func (m Mem) Open(name string, flag int, perm FileMode) (File, error) {
	name = m.join(name)

	f, ok := m.m[name]
	if !ok {
		if flag&os.O_CREATE != 0 {
			f = &memFile{
				info: memFileInfo{
					name:    name,
					size:    0,
					mode:    perm,
					modtime: time.Now(),
					isdir:   false,
				},
			}
			m.m[name] = f
		} else {
			return nil, ErrNotExist
		}
	}

	if flag&os.O_TRUNC != 0 {
		f.b = nil
		f.info.size = 0
	}

	return f, nil
}

func (m Mem) Move(oldname, newname string) error {
	oldname = m.join(oldname)
	newname = m.join(newname)

	f, ok := m.m[oldname]
	if !ok {
		return ErrNotExist
	}

	m.m[newname] = f
	delete(m.m, oldname)

	return nil
}

func (m Mem) Remove(path string) error {
	delete(m.m, m.join(path))

	return nil
}

func (m Mem) RemoveAll(path string) error {
	path = m.join(path)

	for k, file := range m.m {
		if strings.HasPrefix(file.Name(), path) {
			delete(m.m, k)
		}
	}

	return nil
}

func (m Mem) Mkdir(name string, mode FileMode) error {
	return nil
}

func (m Mem) MkdirAll(name string, mode FileMode) error {
	return nil
}

func (m Mem) At(name string) FS {
	m.prefix = m.join(name)

	return m
}

var _ FS = (*Mem)(nil)
