package enginee2e

import (
	"context"
	"os"
	"testing"

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

func TestDepsCache(t *testing.T) {
	ctx := context.Background()

	dir, err := os.MkdirTemp("", "")
	require.NoError(t, err)
	defer os.RemoveAll(dir)

	e, err := engine.New(ctx, dir, engine.Config{})
	require.NoError(t, err)

	staticprovider := pluginstaticprovider.New([]pluginstaticprovider.Target{
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "child",
					Driver:  "sh",
				},
				Config: map[string]*structpb.Value{
					"run": newValueMust([]any{`echo hello > $OUT`}),
					"out": newValueMust([]any{"out_child"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "parent",
					Driver:  "sh",
				},
				Config: map[string]*structpb.Value{
					"run":  newValueMust([]any{`echo $(cat $SRC) > $OUT`}),
					"deps": newValueMust([]any{"//:child"}),
					"out":  newValueMust([]any{"out_parent"}),
				},
			},
		},
	})

	_, err = e.RegisterProvider(ctx, staticprovider)
	require.NoError(t, err)

	_, err = e.RegisterDriver(ctx, pluginexec.New(), nil)
	require.NoError(t, err)

	_, err = e.RegisterDriver(ctx, pluginexec.NewSh(), nil)
	require.NoError(t, err)

	assertOut := func(res *engine.ExecuteResult) {
		fs := hfstest.New(t)
		err = hartifact.Unpack(ctx, res.Outputs[0].Artifact, fs)
		require.NoError(t, res.Err)

		b, err := hfs.ReadFile(fs, "out_parent")
		require.NoError(t, err)

		assert.Equal(t, "hello\n", string(b))
	}

	{ // This will run all
		ch := e.Result(ctx, "", "parent", []string{}, engine.ResultOptions{})

		res := <-ch
		require.NoError(t, res.Err)

		assertOut(res)
	}

	{ // this should reuse cache from deps
		ch := e.Result(ctx, "", "parent", []string{}, engine.ResultOptions{})

		res := <-ch
		require.NoError(t, res.Err)

		assertOut(res)
	}
}
