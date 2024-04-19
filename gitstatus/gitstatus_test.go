package gitstatus

import (
	"context"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
)

func TestIsDirty(t *testing.T) {
	t.SkipNow() // fails in CI, run locally

	dir, err := os.MkdirTemp("", "")
	require.NoError(t, err)

	defer os.RemoveAll(dir)

	ctx := context.Background()
	gs := New(dir)

	git := func(t *testing.T, args ...string) {
		cmd := exec.Command("git", args...)
		cmd.Dir = dir
		err = cmd.Run()
		require.NoError(t, err)
	}

	write := func(t *testing.T, file, content string) {
		err = os.WriteFile(filepath.Join(dir, file), []byte(content), os.ModePerm)
		require.NoError(t, err)
		gs.Reset()
	}

	write(t, "file1", "a")
	write(t, "file2", "a")
	write(t, "file3", "a")

	git(t, "init")
	git(t, "add", ".")
	git(t, "commit", "-m", "initial")

	assert.False(t, gs.IsDirty(ctx, filepath.Join(dir, "file1")))
	assert.False(t, gs.IsDirty(ctx, filepath.Join(dir, "file2")))
	assert.False(t, gs.IsDirty(ctx, filepath.Join(dir, "file3")))
	assert.False(t, gs.IsDirty(ctx, filepath.Join(dir, "doesnt_exist")))

	write(t, "file2", "b")

	assert.False(t, gs.IsDirty(ctx, filepath.Join(dir, "file1")))
	assert.True(t, gs.IsDirty(ctx, filepath.Join(dir, "file2")))
	assert.False(t, gs.IsDirty(ctx, filepath.Join(dir, "file3")))
	assert.False(t, gs.IsDirty(ctx, filepath.Join(dir, "doesnt_exist")))

	git(t, "add", ".")

	assert.False(t, gs.IsDirty(ctx, filepath.Join(dir, "file1")))
	assert.True(t, gs.IsDirty(ctx, filepath.Join(dir, "file2")))
	assert.False(t, gs.IsDirty(ctx, filepath.Join(dir, "file3")))
	assert.False(t, gs.IsDirty(ctx, filepath.Join(dir, "doesnt_exist")))
}
