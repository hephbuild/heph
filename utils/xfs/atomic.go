package xfs

import (
	"github.com/hephbuild/heph/utils/instance"
	"github.com/hephbuild/heph/utils/xrand"
	"os"
)

func ProcessUniquePath(p string) string {
	return p + "_tmp_" + instance.UID + "_" + xrand.RandStr(7)
}

type AtomicFile struct {
	tmpname string
	name    string
	*os.File
}

func (f *AtomicFile) Close() error {
	defer os.Remove(f.tmpname)

	err := f.File.Close()
	if err != nil {
		return err
	}

	return os.Rename(f.tmpname, f.name)
}

func AtomicCreate(name string) (*AtomicFile, error) {
	tmpname := ProcessUniquePath(name)

	f, err := os.Create(tmpname)
	if err != nil {
		return nil, err
	}

	return &AtomicFile{
		tmpname: tmpname,
		name:    name,
		File:    f,
	}, nil
}
