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

func TestDepsCache(t *testing.T) {
	ctx := context.Background()

	dir := t.TempDir()

	e, err := engine.New(ctx, dir, engine.Config{})
	require.NoError(t, err)

	staticprovider := pluginstaticprovider.New([]pluginstaticprovider.Target{
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "child",
				},
				Driver: "bash",
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{`echo hello > $OUT`}),
					"out": hstructpb.NewStringsValue([]string{"out_child"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "",
					Name:    "parent",
				},
				Driver: "bash",
				Config: map[string]*structpb.Value{
					"run":  hstructpb.NewStringsValue([]string{`echo $(cat $SRC) > $OUT`}),
					"deps": hstructpb.NewStringsValue([]string{"//:child"}),
					"out":  hstructpb.NewStringsValue([]string{"out_parent"}),
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
	_, err = e.RegisterDriver(ctx, pluginexec.NewBash(), nil)
	require.NoError(t, err)

	assertOut := func(res *engine.ExecuteResultLocks) {
		fs := hfstest.New(t)
		err = hartifact.Unpack(ctx, res.FindOutputs("")[0].Artifact, fs)
		require.NoError(t, err)

		b, err := hfs.ReadFile(fs, "out_parent")
		require.NoError(t, err)

		assert.Equal(t, "hello\n", string(b))
	}

	{ // This will run all
		res, err := e.Result(ctx, "", "parent", []string{engine.AllOutputs}, engine.ResultOptions{}, &engine.ResolveCache{})
		require.NoError(t, err)
		defer res.Unlock(ctx)

		assertOut(res)
		res.Unlock(ctx)
	}

	{ // this should reuse cache from deps
		res, err := e.Result(ctx, "", "parent", []string{engine.AllOutputs}, engine.ResultOptions{}, &engine.ResolveCache{})
		require.NoError(t, err)
		defer res.Unlock(ctx)

		assertOut(res)
		res.Unlock(ctx)
	}
}
