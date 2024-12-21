package hfs

import (
	"github.com/hephbuild/heph/utils/instance"
	"github.com/hephbuild/heph/utils/xrand"
)

func processUniquePath(p string) string {
	return p + "_tmp_" + instance.UID + "_" + xrand.RandStr(7)
}

type AtomicFile struct {
	tmpname string
	name    string
	fs      FS
	File
}

func (f *AtomicFile) Close() error {
	defer f.fs.Remove(f.tmpname)

	err := f.File.Close()
	if err != nil {
		return err
	}

	return f.fs.Move(f.tmpname, f.name)
}

func AtomicCreate(fs FS, name string) (*AtomicFile, error) {
	tmpname := processUniquePath(name)

	f, err := Create(fs, tmpname)
	if err != nil {
		return nil, err
	}

	return &AtomicFile{
		tmpname: tmpname,
		name:    name,
		File:    f,
	}, nil
}
