package hfs

import (
	"io/fs"
	"os"
	"testing"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func assertSame(t *testing.T, fss []container, f func(*testing.T, container) []any) {
	t.Helper()

	var expected []any
	for i, c := range fss {
		if i == 0 {
			expected = f(t, c)
		} else {
			actual := f(t, c)

			require.Len(t, actual, len(expected))

			for i, expected := range expected {
				actual := actual[i]

				assert.Equalf(t, expected, actual, "fs %T, at index %v", c.fs, i)
			}
		}
	}
}

type container struct {
	fs FS
	f  File
}

func doSame(t *testing.T, fss []FS, f func(*testing.T, FS) File) []container {
	t.Helper()

	res := make([]container, 0, len(fss))
	for _, fs := range fss {
		res = append(res, container{fs: fs, f: f(t, fs)})
	}

	return res
}

func TestSanity(t *testing.T) {
	dir := t.TempDir()

	fss := []FS{
		NewOS(dir),
	}

	files := doSame(t, fss, func(t *testing.T, fs FS) File {
		f, err := Create(fs, "some/file")
		require.NoError(t, err)

		n, err := f.Write([]byte(`hello, world`))
		require.NoError(t, err)

		assert.Equal(t, 12, n)

		return f
	})

	assertSame(t, files, func(t *testing.T, c container) []any {
		info, err := c.f.Stat()
		require.NoError(t, err)

		return []any{info.Size(), info.IsDir()}
	})

	assertSame(t, files, func(t *testing.T, c container) []any {
		err := c.f.Close()
		require.NoError(t, err)

		return nil
	})

	assertSame(t, files, func(t *testing.T, c container) []any {
		b, err := ReadFile(c.fs, "some/file")
		require.NoError(t, err)

		return []any{b}
	})

	assertSame(t, files, func(t *testing.T, c container) []any {
		err := WriteFile(c.fs, "some/file", []byte(`foo, bar`), ModePerm)
		require.NoError(t, err)

		return nil
	})

	assertSame(t, files, func(t *testing.T, c container) []any {
		b, err := ReadFile(c.fs, "some/file")
		require.NoError(t, err)

		return []any{b}
	})

	assertSame(t, files, func(t *testing.T, c container) []any {
		info, err := c.fs.Stat("")
		require.NoError(t, err)

		require.True(t, info.IsDir())

		return []any{info.IsDir()}
	})

	assertSame(t, files, func(t *testing.T, c container) []any {
		info, err := c.fs.Stat("some")
		require.NoError(t, err)

		return []any{info.IsDir()}
	})

	assertSame(t, files, func(t *testing.T, c container) []any {
		var paths []string
		err := Walk(c.fs, func(path string, d fs.DirEntry, err error) error {
			if err != nil {
				return err
			}

			paths = append(paths, path)

			return nil
		})
		require.NoError(t, err)

		return []any{paths}
	})
}

func TestSanityAt(t *testing.T) {
	dir := t.TempDir()

	fss := []FS{
		At(NewOS(dir), "some/dir"),
	}

	files := doSame(t, fss, func(t *testing.T, fs FS) File {
		err := WriteFile(fs, "some/file", []byte(`hello, world`), os.ModePerm)
		require.NoError(t, err)

		return nil
	})

	assertSame(t, files, func(t *testing.T, c container) []any {
		b, err := ReadFile(c.fs, "some/file")
		require.NoError(t, err)

		return []any{b}
	})
}
