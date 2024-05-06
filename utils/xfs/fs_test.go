package xfs

import (
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"os"
	"path/filepath"
	"testing"
)

func writeFile(t *testing.T, tmp, name string, mode os.FileMode) {
	t.Helper()

	path := filepath.Join(tmp, name)

	t.Logf("writing file %s: %v", name, mode.String())
	err := os.WriteFile(path, []byte("hello"), os.ModePerm)
	require.NoError(t, err)

	err = os.Chmod(path, mode)
	require.NoError(t, err)
}

func isExec(t *testing.T, tmp, name string) bool {
	info, err := os.Stat(filepath.Join(tmp, name))
	require.NoError(t, err)

	return info.Mode()&0111 == 0111
}

func TestROFS(t *testing.T) {
	dir, err := os.MkdirTemp("", "")
	require.NoError(t, err)
	defer func() {
		MakeDirsReadWrite(dir)
		os.RemoveAll(dir)
	}()

	t.Log(dir)

	writeFile(t, dir, "non_exec", os.FileMode(0666))
	writeFile(t, dir, "exec", os.FileMode(0777))

	assert.False(t, isExec(t, dir, "non_exec"))
	assert.True(t, isExec(t, dir, "exec"))

	MakeDirsReadOnly(dir)

	assert.False(t, isExec(t, dir, "non_exec"))
	assert.True(t, isExec(t, dir, "exec"))

	err = os.Remove(filepath.Join(dir, "non_exec"))
	assert.ErrorIs(t, err, os.ErrPermission)
	err = os.Remove(filepath.Join(dir, "exec"))
	assert.ErrorIs(t, err, os.ErrPermission)

	MakeDirsReadWrite(dir)

	assert.False(t, isExec(t, dir, "non_exec"))
	assert.True(t, isExec(t, dir, "exec"))

	err = os.Remove(filepath.Join(dir, "non_exec"))
	assert.NoError(t, err)
	err = os.Remove(filepath.Join(dir, "exec"))
	assert.NoError(t, err)
}
