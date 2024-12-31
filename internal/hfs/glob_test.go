package hfs_test

import (
	"context"
	"os"
	"slices"
	"testing"

	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hfs/hfstest"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func setup(t *testing.T, paths []string) hfs.FS {
	fs := hfstest.New(t)
	for _, path := range paths {
		err := hfs.CreateParentDir(fs, path)
		require.NoError(t, err)

		err = hfs.WriteFile(fs, path, nil, os.ModePerm)
		require.NoError(t, err)
	}

	return fs
}

func collector() (hfs.GlobWalkFunc, func() []string) {
	var matches []string

	return func(path string, d hfs.DirEntry) error {
			matches = append(matches, path)

			return nil
		}, func() []string {
			slices.Sort(matches)
			return matches
		}
}

func TestGlobNoPattern(t *testing.T) {
	fs := setup(t, []string{
		"file1",
		"some/file2",
		"some/deep/file3",
	})

	fn, get := collector()

	err := hfs.Glob(context.Background(), fs, "", nil, fn)
	require.NoError(t, err)

	assert.EqualValues(t, []string{
		"file1",
		"some/deep/file3",
		"some/file2",
	}, get())
}

func TestGlobAllPattern(t *testing.T) {
	fs := setup(t, []string{
		"file1",
		"some/file2",
		"some/deep/file3",
	})

	fn, get := collector()

	err := hfs.Glob(context.Background(), fs, "**/*", nil, fn)
	require.NoError(t, err)

	assert.EqualValues(t, []string{
		"file1",
		"some/deep/file3",
		"some/file2",
	}, get())
}

func TestGlobAllFirstLevelPattern(t *testing.T) {
	fs := setup(t, []string{
		"file1",
		"some/file2",
		"some/deep/file3",
	})

	fn, get := collector()

	err := hfs.Glob(context.Background(), fs, "*", nil, fn)
	require.NoError(t, err)

	assert.EqualValues(t, []string{
		"file1",
	}, get())
}

func TestGlobAllFirstSecondLevelPattern(t *testing.T) {
	fs := setup(t, []string{
		"file1",
		"some/file2",
		"some/deep/file3",
	})

	fn, get := collector()

	err := hfs.Glob(context.Background(), fs, "some/*", nil, fn)
	require.NoError(t, err)

	assert.EqualValues(t, []string{
		"some/file2",
	}, get())
}

func TestGlobNoPatternSecondLevel(t *testing.T) {
	fs := setup(t, []string{
		"file1",
		"some/file2",
		"some/deep/file3",
	})

	fn, get := collector()

	err := hfs.Glob(context.Background(), fs, "some", nil, fn)
	require.NoError(t, err)

	assert.EqualValues(t, []string{
		"some/deep/file3",
		"some/file2",
	}, get())
}
