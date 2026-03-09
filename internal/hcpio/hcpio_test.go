package hcpio

import (
	"bytes"
	"context"
	"os"
	"path/filepath"
	"testing"

	"github.com/hephbuild/heph/internal/hfs"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestPackUnpack(t *testing.T) {
	ctx := context.Background()

	tmpDir, err := os.MkdirTemp("", "hcpio_test")
	require.NoError(t, err)
	defer os.RemoveAll(tmpDir)

	sourceDir := filepath.Join(tmpDir, "source")
	err = os.MkdirAll(sourceDir, 0755)
	require.NoError(t, err)

	// Create a regular file
	file1Path := filepath.Join(sourceDir, "file1.txt")
	err = os.WriteFile(file1Path, []byte("hello world"), 0644)
	require.NoError(t, err)

	// Create a relative symlink
	link1Path := filepath.Join(sourceDir, "link1.txt")
	err = os.Symlink("file1.txt", link1Path)
	require.NoError(t, err)

	// Create a subdirectory with another relative symlink
	subDir := filepath.Join(sourceDir, "subdir")
	err = os.MkdirAll(subDir, 0755)
	require.NoError(t, err)

	file2Path := filepath.Join(subDir, "file2.txt")
	err = os.WriteFile(file2Path, []byte("hello subdir"), 0644)
	require.NoError(t, err)

	link2Path := filepath.Join(subDir, "link2.txt")
	err = os.Symlink("file2.txt", link2Path) // relative to subdir
	require.NoError(t, err)

	link3Path := filepath.Join(subDir, "link3.txt")
	err = os.Symlink("../file1.txt", link3Path) // relative to subdir, pointing to parent
	require.NoError(t, err)

	// Pack it
	var buf bytes.Buffer
	packer := NewPacker(&buf)

	sourceFS := hfs.NewOS(sourceDir)

	files := []string{
		"file1.txt",
		"link1.txt",
		"subdir",
		"subdir/file2.txt",
		"subdir/link2.txt",
		"subdir/link3.txt",
	}

	for _, relPath := range files {
		node := sourceFS.At(relPath)
		f, err := node.Open(os.O_RDONLY, 0)
		require.NoError(t, err, relPath)
		err = packer.WriteFile(f, relPath)
		require.NoError(t, err, relPath)
		f.Close()
	}

	err = packer.Close()
	require.NoError(t, err)

	// Unpack it
	targetDir := filepath.Join(tmpDir, "target")
	err = os.MkdirAll(targetDir, 0755)
	require.NoError(t, err)

	targetFS := hfs.NewOS(targetDir)
	err = Unpack(ctx, &buf, targetFS)
	require.NoError(t, err)

	// Verify
	content, err := os.ReadFile(filepath.Join(targetDir, "file1.txt"))
	require.NoError(t, err)
	assert.Equal(t, "hello world", string(content))

	link1Target, err := os.Readlink(filepath.Join(targetDir, "link1.txt"))
	require.NoError(t, err)
	assert.Equal(t, "file1.txt", link1Target)

	content2, err := os.ReadFile(filepath.Join(targetDir, "subdir/file2.txt"))
	require.NoError(t, err)
	assert.Equal(t, "hello subdir", string(content2))

	link2Target, err := os.Readlink(filepath.Join(targetDir, "subdir/link2.txt"))
	require.NoError(t, err)
	assert.Equal(t, "file2.txt", link2Target)

	link3Target, err := os.Readlink(filepath.Join(targetDir, "subdir/link3.txt"))
	require.NoError(t, err)
	assert.Equal(t, "../file1.txt", link3Target)
}

func TestPackAbsLink(t *testing.T) {
	tmpDir, err := os.MkdirTemp("", "hcpio_test_abs")
	require.NoError(t, err)
	defer os.RemoveAll(tmpDir)

	sourceDir := filepath.Join(tmpDir, "source")
	err = os.MkdirAll(sourceDir, 0755)
	require.NoError(t, err)

	absTarget := filepath.Join(tmpDir, "abs_target.txt")
	err = os.WriteFile(absTarget, []byte("abs"), 0644)
	require.NoError(t, err)

	linkPath := filepath.Join(sourceDir, "abs_link.txt")
	err = os.Symlink(absTarget, linkPath)
	require.NoError(t, err)

	sourceFS := hfs.NewOS(sourceDir)

	// Should fail with default Packer
	var buf bytes.Buffer
	packer := NewPacker(&buf)
	node := sourceFS.At("abs_link.txt")
	f, err := node.Open(os.O_RDONLY, 0)
	require.NoError(t, err)
	defer f.Close()

	err = packer.WriteFile(f, "abs_link.txt")
	assert.Error(t, err)
	assert.Contains(t, err.Error(), "absolute link not allowed")

	// Should succeed if AllowAbsLink is true
	buf.Reset()
	packer = NewPacker(&buf)
	packer.AllowAbsLink = true
	err = packer.WriteFile(f, "abs_link.txt")
	assert.NoError(t, err)
	packer.Close()
}
