package htar

import (
	"bytes"
	"path"
	"testing"

	"github.com/go-faker/faker/v4"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hfs/hfstest"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestSanity(t *testing.T) {
	ctx := t.Context()

	srcfs := hfstest.New(t)

	{
		f, err := hfs.Create(srcfs.At("file1"))
		require.NoError(t, err)

		_, err = f.Write([]byte(`hello, world`))
		require.NoError(t, err)

		err = f.Close()
		require.NoError(t, err)
	}

	var b bytes.Buffer

	{
		p := NewPacker(&b)
		defer p.Close()

		f, err := hfs.Open(srcfs.At("file1"))
		require.NoError(t, err)

		err = p.WriteFile(f, "some/file1")
		require.NoError(t, err)

		err = f.Close()
		require.NoError(t, err)

		err = p.Close()
		require.NoError(t, err)
	}

	{
		dstfs := hfstest.New(t)

		err := Unpack(ctx, &b, dstfs)
		require.NoError(t, err)

		b, err := hfs.ReadFile(dstfs.At("some/file1"))
		require.NoError(t, err)

		assert.Equal(t, "hello, world", string(b))
	}
}

func TestSanitySymlink(t *testing.T) {
	ctx := t.Context()

	srcfs := hfstest.New(t)

	{
		f, err := hfs.Create(srcfs.At("file1"))
		require.NoError(t, err)

		_, err = f.Write([]byte(`hello, world`))
		require.NoError(t, err)

		err = f.Close()
		require.NoError(t, err)

		err = srcfs.At("./file1.link").(hfs.OS).Symlink("file1")
		require.NoError(t, err)
	}

	var b bytes.Buffer

	{
		p := NewPacker(&b)
		defer p.Close()

		f, err := hfs.Open(srcfs.At("file1.link"))
		require.NoError(t, err)

		err = p.WriteFile(f, "some/file1.link")
		require.NoError(t, err)

		err = f.Close()
		require.NoError(t, err)

		err = p.Close()
		require.NoError(t, err)
	}

	{
		dstfs := hfstest.New(t)

		err := Unpack(ctx, &b, dstfs)
		require.NoError(t, err)

		dst, err := dstfs.At("some/file1.link").(hfs.OS).Readlink()
		require.NoError(t, err)

		assert.Equal(t, "file1", dst)
	}
}

func fakePath() string {
	var names []string
	n, _ := faker.RandomInt(1, 100, 1)
	if len(n) != 1 {
		panic("not supposed to happen")
	}
	for range n[0] {
		names = append(names, faker.Username())
	}

	return path.Join(names...)
}

func TestMonkey(t *testing.T) {
	ctx := t.Context()

	srcfs := hfstest.New(t)

	var paths []string
	pathscontent := map[string]string{}
	for range 100 {
		filepath := fakePath()
		paths = append(paths, filepath)

		content := faker.Paragraph()
		pathscontent[filepath] = content

		err := hfs.WriteFile(srcfs.At(filepath), []byte(content))
		require.NoError(t, err)
	}

	var b bytes.Buffer

	{
		p := NewPacker(&b)
		defer p.Close()

		for _, filepath := range paths {
			f, err := hfs.Open(srcfs.At(filepath))
			require.NoError(t, err)

			err = p.WriteFile(f, filepath)
			require.NoError(t, err)

			err = f.Close()
			require.NoError(t, err)
		}

		err := p.Close()
		require.NoError(t, err)
	}

	{
		dstfs := hfstest.New(t)

		err := Unpack(ctx, &b, dstfs)
		require.NoError(t, err)

		for _, filepath := range paths {
			b, err := hfs.ReadFile(dstfs.At(filepath))
			require.NoError(t, err)

			assert.Equal(t, pathscontent[filepath], string(b))
		}
	}
}
