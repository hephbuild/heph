package hfstest

import (
	"github.com/hephbuild/hephv2/internal/hfs"
	"testing"
)

func New(t *testing.T) hfs.FS {
	root := t.TempDir()

	return hfs.NewOS(root)
}
