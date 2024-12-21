package plugingroup

import (
	"connectrpc.com/connect"
	"context"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	groupv1 "github.com/hephbuild/hephv2/plugin/pluginsh/gen/heph/plugin/group/v1"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"google.golang.org/protobuf/encoding/protojson"
	"google.golang.org/protobuf/types/known/anypb"
	"os"
	"testing"
)

func TestSanity(t *testing.T) {
	ctx := context.Background()
	sandboxPath, err := os.MkdirTemp("", "")
	require.NoError(t, err)

	p := New()

	{
		res, err := p.Config(ctx, connect.NewRequest(&pluginv1.ConfigRequest{}))
		require.NoError(t, err)

		b, err := protojson.Marshal(res.Msg.TargetSchema)
		require.NoError(t, err)
		require.NotEmpty(t, b)
		//require.JSONEq(t, `{"name":"Target", "field":[{"name":"run", "number":1, "label":"LABEL_REPEATED", "type":"TYPE_STRING", "jsonName":"run"}]}`, string(b))
	}

	{
		def, err := anypb.New(&groupv1.Target{
			Run: []string{"echo", "hello"},
		})
		require.NoError(t, err)

		res, err := p.Run(ctx, connect.NewRequest(&pluginv1.RunRequest{
			Target: &pluginv1.TargetDef{
				Ref: &pluginv1.TargetRef{
					Addr:   "some/pkg:target",
					Driver: "sh",
				},
				Def: def,
			},
			SandboxPath: sandboxPath,
		}))
		require.NoError(t, err)

		assert.Len(t, res.Msg.Artifacts, 1)
	}
}
