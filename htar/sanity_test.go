package htar

import (
	"bytes"
	"context"
	"github.com/go-faker/faker/v4"
	"github.com/hephbuild/hephv2/hfs"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"path"
	"testing"
)

func TestSanity(t *testing.T) {
	ctx := context.Background()

	srcfs := hfs.NewMem()

	f, err := hfs.Create(srcfs, "file1")
	require.NoError(t, err)

	_, err = f.Write([]byte(`hello, world`))
	require.NoError(t, err)

	var b bytes.Buffer

	{
		p := NewPacker(&b)
		defer p.Close()

		err = p.WriteFile(f, "some/file1")
		require.NoError(t, err)

		err = p.Close()
		require.NoError(t, err)
	}

	{
		dstfs := hfs.NewMem()

		err = Unpack(ctx, &b, dstfs)
		require.NoError(t, err)

		b, err := hfs.ReadFile(dstfs, "some/file1")
		require.NoError(t, err)

		assert.Equal(t, "hello, world", string(b))
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
	ctx := context.Background()

	srcfs := hfs.NewMem()

	var paths []string
	pathscontent := map[string]string{}
	for range 100 {
		filepath := fakePath()
		paths = append(paths, filepath)

		content := faker.Paragraph()
		pathscontent[filepath] = content

		err := hfs.WriteFile(srcfs, filepath, []byte(content), hfs.ModePerm)
		require.NoError(t, err)
	}

	var b bytes.Buffer

	{
		p := NewPacker(&b)
		defer p.Close()

		for _, filepath := range paths {
			f, err := hfs.Open(srcfs, filepath)
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
		dstfs := hfs.NewMem()

		err := Unpack(ctx, &b, dstfs)
		require.NoError(t, err)

		for _, filepath := range paths {
			b, err := hfs.ReadFile(dstfs, filepath)
			require.NoError(t, err)

			assert.Equal(t, pathscontent[filepath], string(b))
		}
	}
}
