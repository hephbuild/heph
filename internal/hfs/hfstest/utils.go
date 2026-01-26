package hfstest

import (
	"testing"

	"github.com/hephbuild/heph/internal/hfs"
)

func New(t *testing.T) hfs.OS {
	root := t.TempDir()

	return hfs.NewOS(root)
}
