package pluginbuildfile

import (
	"os"
	"testing"

	"github.com/hephbuild/heph/internal/htypes"

	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hfs/hfstest"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestSanity(t *testing.T) {
	ctx := t.Context()

	fs := hfstest.New(t)

	err := hfs.WriteFile(fs, "BUILD", []byte(`target(name="hello", driver="exec", run=["hello"])`), os.ModePerm)
	require.NoError(t, err)

	p := New(fs, Options{Patterns: []string{"BUILD"}})

	var ref *pluginv1.TargetRef
	{
		res, err := p.List(ctx, pluginv1.ListRequest_builder{
			Package: htypes.Ptr(""),
		}.Build())
		require.NoError(t, err)

		require.True(t, res.Receive())
		spec := res.Msg().GetSpec()
		ref = spec.GetRef()

		assert.Empty(t, ref.GetPackage())
		assert.Equal(t, "hello", ref.GetName())
	}

	{
		res, err := p.Get(ctx, pluginv1.GetRequest_builder{
			Ref: ref,
		}.Build())
		require.NoError(t, err)

		assert.Equal(t, []any{"hello"}, res.GetSpec().GetConfig()["run"].GetListValue().AsSlice())
	}
}

func TestLoad(t *testing.T) {
	ctx := t.Context()

	fs := hfstest.New(t)

	err := hfs.WriteFile(fs, "BUILD", []byte(`load("//some/deep", "myvar"); target(name="hello", driver="exec", run=[myvar])`), os.ModePerm)
	require.NoError(t, err)

	err = hfs.WriteFile(fs, "some/deep/BUILD", []byte(`myvar = "hello"`), os.ModePerm)
	require.NoError(t, err)

	p := New(fs, Options{Patterns: []string{"BUILD"}})

	var ref *pluginv1.TargetRef
	{
		res, err := p.List(ctx, pluginv1.ListRequest_builder{
			Package: htypes.Ptr(""),
		}.Build())
		require.NoError(t, err)

		assert.True(t, res.Receive())
		require.NoError(t, res.Err())
		spec := res.Msg().GetSpec()
		ref = spec.GetRef()

		assert.Empty(t, ref.GetPackage())
		assert.Equal(t, "hello", ref.GetName())
	}

	{
		res, err := p.Get(ctx, pluginv1.GetRequest_builder{
			Ref: ref,
		}.Build())
		require.NoError(t, err)

		assert.Equal(t, []any{"hello"}, res.GetSpec().GetConfig()["run"].GetListValue().AsSlice())
	}
}

func TestLoad(t *testing.T) {
	ctx := t.Context()

	fs := hfstest.New(t)

	err := hfs.WriteFile(fs, "BUILD", []byte(`load("//some/deep", "myvar"); target(name="hello", driver="exec", run=[myvar])`), os.ModePerm)
	require.NoError(t, err)

	err = hfs.WriteFile(fs, "some/deep/BUILD", []byte(`myvar = "hello"`), os.ModePerm)
	require.NoError(t, err)

	p := New(fs, Options{Patterns: []string{"BUILD"}})

	var ref *pluginv1.TargetRef
	{
		res, err := p.List(ctx, &pluginv1.ListRequest{
			Package: "",
		})
		require.NoError(t, err)

		assert.True(t, res.Receive())
		require.NoError(t, res.Err())
		spec := res.Msg().GetSpec()
		ref = spec.GetRef()

		assert.Empty(t, ref.GetPackage())
		assert.Equal(t, "hello", ref.GetName())
	}

	{
		res, err := p.Get(ctx, &pluginv1.GetRequest{
			Ref: ref,
		})
		require.NoError(t, err)

		assert.Equal(t, []any{"hello"}, res.GetSpec().GetConfig()["run"].GetListValue().AsSlice())
	}
}
