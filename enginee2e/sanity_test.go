package enginee2e

import (
	"context"
	"github.com/hephbuild/hephv2/engine"
	"github.com/hephbuild/hephv2/hfs"
	"github.com/hephbuild/hephv2/htar"
	pluginv1 "github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/hephv2/plugin/pluginsh"
	"github.com/hephbuild/hephv2/plugin/pluginstaticprovider"
	"github.com/hephbuild/hephv2/plugin/plugintest"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"google.golang.org/protobuf/types/known/structpb"
	"os"
	"strings"
	"testing"
)

func newValueMust(v any) *structpb.Value {
	pv, err := structpb.NewValue(v)
	if err != nil {
		panic(err)
	}

	return pv
}

func TestSanity(t *testing.T) {
	ctx := context.Background()

	dir, err := os.MkdirTemp("", "")
	require.NoError(t, err)
	defer os.RemoveAll(dir)

	e, err := engine.New(dir, engine.Config{})
	require.NoError(t, err)

	staticprovider := pluginstaticprovider.New([]pluginstaticprovider.Target{
		{
			Ref: &pluginv1.TargetRef{
				Package: "some/package",
				Name:    "sometarget",
				Driver:  "sh",
			},
			Spec: &pluginv1.TargetSpec{
				Config: map[string]*structpb.Value{
					"run": newValueMust([]any{"sh", "-c", `echo hello > out`}),
					"outputs": newValueMust([]any{map[string]any{
						"paths": []any{"out"},
					}}),
				},
			},
		},
	})

	staticproviderClient := plugintest.ProviderClient(t, staticprovider)

	err = e.RegisterProvider(ctx, staticproviderClient)
	require.NoError(t, err)

	execdriver := pluginsh.New()
	err = e.RegisterDriver(ctx, execdriver)
	require.NoError(t, err)

	ch := e.Result(ctx, "some/package", "sometarget", []string{""})

	res := <-ch
	require.NoError(t, res.Err)

	require.Len(t, res.Outputs, 1)

	path := strings.TrimPrefix(res.Outputs[0].Uri, "file://")

	fs2 := hfs.NewMem()
	err = htar.UnpackFromPath(ctx, path, fs2)
	require.NoError(t, res.Err)

	b, err := hfs.ReadFile(fs2, "ws/out")
	require.NoError(t, res.Err)

	assert.Equal(t, "hello\n", string(b))
}
