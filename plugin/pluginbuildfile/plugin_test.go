package pluginbuildfile

import (
	"context"
	"os"
	"testing"

	"connectrpc.com/connect"
	"github.com/hephbuild/hephv2/internal/hfs"
	"github.com/hephbuild/hephv2/internal/hfs/hfstest"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/hephv2/plugin/plugintest"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestSanity(t *testing.T) {
	ctx := context.Background()

	fs := hfstest.New(t)

	err := hfs.WriteFile(fs, "BUILD", []byte(`target(name="hello", driver="exec", run=["hello"])`), os.ModePerm)
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
		assert.Equal(t, "", spec.GetRef().GetPackage())
		assert.Equal(t, "hello", spec.GetRef().GetName())
		assert.Equal(t, "exec", spec.GetRef().GetDriver())

		ref = spec.GetRef()
	}

	{
		res, err := pc.Get(ctx, connect.NewRequest(&pluginv1.GetRequest{
			Ref: ref,
		}))
		require.NoError(t, err)

		assert.Equal(t, []any{"hello"}, res.Msg.GetSpec().GetConfig()["run"].GetListValue().AsSlice())
	}
}
