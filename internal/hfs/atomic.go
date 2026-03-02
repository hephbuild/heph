package hfs

import (
	"github.com/hephbuild/heph/internal/hinstance"
	"github.com/hephbuild/heph/internal/hrand"
)

func processUniquePath(p string) string {
	return p + "_tmp_" + hinstance.UID + "_" + hrand.Str(7)
}

type AtomicFile struct {
	tmpNode Node
	dstNode Node
	File
}

func (f *AtomicFile) Close() error {
	defer f.tmpNode.Remove()

	err := f.File.Close()
	if err != nil {
		return err
	}

	return f.tmpNode.Move(f.dstNode)
}

func AtomicCreate(fs Node) (*AtomicFile, error) {
	tmpNode := fs.At(processUniquePath(fs.Path()))

	f, err := Create(tmpNode)
	if err != nil {
		return nil, err
	}

	return &AtomicFile{
		tmpNode: tmpNode,
		dstNode: fs,
		File:    f,
	}, nil
}
