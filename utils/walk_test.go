package utils

import (
	"fmt"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"io/fs"
	"os"
	"testing"
)

func assertWalk(t *testing.T, pattern string, ignored, expected []string) {
	wd, err := os.Getwd()
	require.NoError(t, err)

	paths := make([]string, 0)
	err = StarWalk(wd, pattern, ignored, func(path string, d fs.DirEntry, err error) error {
		paths = append(paths, path)
		return nil
	})
	require.NoError(t, err)

	assert.EqualValues(t, expected, paths)
}

func assertWalk1(t *testing.T, pattern string) {
	assertWalk(t, pattern, nil, []string{"testdata/some_files/a.txt", "testdata/some_files/b.txt"})
}

func assertWalkAll(t *testing.T, pattern string) {
	assertWalk(t, pattern, nil, []string{
		"testdata/more_files/c.txt",
		"testdata/more_files/d.txt",
		"testdata/some_files/a.txt",
		"testdata/some_files/b.txt",
	})
}

func TestStarWalk_1Star(t *testing.T) {
	t.Parallel()

	assertWalk1(t, "testdata/some_files/*")
	assertWalk1(t, "testdata/some*/*")
	assertWalk(t, "testdata/some_files/a.*", nil, []string{"testdata/some_files/a.txt"})
	assertWalk(t, "testdata/some_files/*", []string{
		"testdata/some_files/b.txt",
	}, []string{
		"testdata/some_files/a.txt",
	})
	assertWalk(t, "testdata/some_files/*", []string{
		"**/b.txt",
	}, []string{
		"testdata/some_files/a.txt",
	})
}

func TestStarWalk_1StarStar(t *testing.T) {
	t.Parallel()

	assertWalk1(t, "testdata/some_files/**/*")
}

func TestStarWalk_2Star(t *testing.T) {
	t.Parallel()

	assertWalk(t, "testdata/*", nil, []string{})
}

func TestStarWalk_2StarStar(t *testing.T) {
	t.Parallel()

	assertWalkAll(t, "testdata/**/*")
	assertWalkAll(t, "testdat*/**/*")
	assertWalk(t, "testdata/**/*", []string{
		"**/some_files",
	}, []string{
		"testdata/more_files/c.txt",
		"testdata/more_files/d.txt",
	})
	assertWalk(t, "testdata/**/*", []string{
		"**/a.txt",
	}, []string{
		"testdata/more_files/c.txt",
		"testdata/more_files/d.txt",
		"testdata/some_files/b.txt",
	})
	assertWalk(t, "testdata/**/*", []string{
		"**/a.*",
	}, []string{
		"testdata/more_files/c.txt",
		"testdata/more_files/d.txt",
		"testdata/some_files/b.txt",
	})
}

func TestStarWalk_Dir(t *testing.T) {
	t.Parallel()

	assertWalkAll(t, "testdata")
}

func TestPathMatch(t *testing.T) {
	tests := []struct {
		path     string
		matchers []string
		expected bool
	}{
		{"some/path/to/file", []string{"**/some/**/*"}, true},
		{"some/path/", []string{"**/some/path"}, false},
		{"some/path", []string{"**/some/path"}, true},
		{"some/path/to/file", []string{"**/path/**/*"}, true},
		{"some/path/to/file", []string{"**/to/**/*"}, true},
		{"some/path/to/file", []string{"**/file"}, true},
		{"some/path/to/file", []string{"some/path"}, true},
		{"some/path/to/file", []string{"some/path/to"}, true},
		{"some/path/to/file", []string{"some/path/to/file"}, true},
		{"some/path/to/file", []string{"path/to"}, false},
		{"some/path/to/file", []string{"some/path/*"}, false},
		{"some/path/to/file", []string{"some/path/**/*"}, true},
		{"some/path/to/file", []string{"some/path/to/file/**/*"}, true},
	}
	for _, test := range tests {
		t.Run(fmt.Sprintf("%v %v", test.path, test.matchers), func(t *testing.T) {
			actual, err := PathMatch(test.path, test.matchers...)
			require.NoError(t, err)

			assert.Equal(t, test.expected, actual)
		})
	}
}
