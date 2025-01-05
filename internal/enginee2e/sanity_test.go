package enginee2e

import (
	"context"
	"testing"

	"github.com/hephbuild/heph/internal/hproto/hstructpb"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hfs/hfstest"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/pluginexec"
	"github.com/hephbuild/heph/plugin/pluginstaticprovider"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"google.golang.org/protobuf/types/known/structpb"
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

	dir := t.TempDir()

	e, err := engine.New(ctx, dir, engine.Config{})
	require.NoError(t, err)

	staticprovider := pluginstaticprovider.New([]pluginstaticprovider.Target{
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "some/package",
					Name:    "sometarget",
					Driver:  "exec",
				},
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{"sh", "-c", "-e", `echo hello > out`}),
					"out": hstructpb.NewStringsValue([]string{"out"}),
				},
			},
		},
	})

	_, err = e.RegisterProvider(ctx, staticprovider)
	require.NoError(t, err)

	execdriver := pluginexec.New()
	_, err = e.RegisterDriver(ctx, execdriver, nil)
	require.NoError(t, err)

	ch := e.Result(ctx, "some/package", "sometarget", []string{""}, engine.ResultOptions{})

	res := <-ch
	require.NoError(t, res.Err)

	require.Len(t, res.Artifacts, 2)

	fs2 := hfstest.New(t)
	err = hartifact.Unpack(ctx, res.Artifacts[0].Artifact, fs2)
	require.NoError(t, err)

	b, err := hfs.ReadFile(fs2, "some/package/out")
	require.NoError(t, err)

	assert.Equal(t, "hello\n", string(b))
}
