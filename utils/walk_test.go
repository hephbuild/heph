package utils

import (
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"io/fs"
	"os"
	"testing"
)

func assertWalk(t *testing.T, pattern string, expected []string) {
	wd, err := os.Getwd()
	require.NoError(t, err)

	paths := make([]string, 0)
	err = StarWalk(wd, pattern, nil, func(path string, d fs.DirEntry, err error) error {
		paths = append(paths, path)
		return nil
	})
	require.NoError(t, err)

	assert.EqualValues(t, expected, paths)
}

func assertWalk1(t *testing.T, pattern string) {
	assertWalk(t, pattern, []string{"testdata/some_files/a.txt", "testdata/some_files/b.txt"})
}

func TestStarWalk_1Star(t *testing.T) {
	t.Parallel()

	assertWalk1(t, "testdata/some_files/*")
}

func TestStarWalk_1StarStar(t *testing.T) {
	t.Parallel()

	assertWalk1(t, "testdata/some_files/**/*")
}

func TestStarWalk_2Star(t *testing.T) {
	t.Parallel()

	assertWalk(t, "testdata/*", []string{"testdata/more_files", "testdata/some_files"})
}

func TestStarWalk_2StarStar(t *testing.T) {
	t.Parallel()

	assertWalk(t, "testdata/**/*", []string{
		"testdata/more_files",
		"testdata/more_files/c.txt",
		"testdata/more_files/d.txt",
		"testdata/some_files",
		"testdata/some_files/a.txt",
		"testdata/some_files/b.txt",
	})
}
