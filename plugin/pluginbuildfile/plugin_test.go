package pluginbuildfile

import (
	"connectrpc.com/connect"
	"context"
	"github.com/hephbuild/hephv2/hfs"
	"github.com/hephbuild/hephv2/hfs/hfstest"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/hephv2/plugin/plugintest"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"os"
	"testing"
)

func TestSanity(t *testing.T) {
	ctx := context.Background()

	fs := hfstest.New(t)

	err := hfs.WriteFile(fs, "BUILD", []byte(`target(name="hello", driver="sh", run=["hello"])`), os.ModePerm)
	require.NoError(t, err)

	p := New(fs)

	pc := plugintest.ProviderClient(t, p)

	var ref *pluginv1.TargetRef
	{
		res, err := pc.List(ctx, connect.NewRequest(&pluginv1.ListRequest{
			Package: "",
			Deep:    true,
		}))
		require.NoError(t, err)

		require.True(t, res.Receive())
		spec := res.Msg()
		assert.Equal(t, "", spec.Ref.Package)
		assert.Equal(t, "hello", spec.Ref.Name)
		assert.Equal(t, "sh", spec.Ref.Driver)

		ref = spec.Ref
	}

	{
		res, err := pc.Get(ctx, connect.NewRequest(&pluginv1.GetRequest{
			Ref: ref,
		}))
		require.NoError(t, err)

		assert.Equal(t, []any{"hello"}, res.Msg.Spec.Config["run"].GetListValue().AsSlice())
	}
}
