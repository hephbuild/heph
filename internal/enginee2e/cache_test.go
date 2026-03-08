package enginee2e

import (
	"testing"

	"github.com/hephbuild/heph/internal/htypes"

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

func TestCache(t *testing.T) {
	ctx := t.Context()

	dir := t.TempDir()

	e, err := engine.New(ctx, dir, engine.Config{})
	require.NoError(t, err)

	staticprovider := pluginstaticprovider.New([]pluginstaticprovider.Target{
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(""),
					Name:    htypes.Ptr("child"),
				}.Build(),
				Driver: htypes.Ptr("bash"),
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{`echo hello > $OUT`}),
					"out": hstructpb.NewStringsValue([]string{"out_child"}),
				},
			}.Build(),
		},
	})

	_, err = e.RegisterProvider(ctx, staticprovider, engine.RegisterProviderConfig{})
	require.NoError(t, err)

	_, err = e.RegisterDriver(ctx, pluginexec.NewExec(), nil)
	require.NoError(t, err)
	_, err = e.RegisterDriver(ctx, pluginexec.NewSh(), nil)
	require.NoError(t, err)
	_, err = e.RegisterDriver(ctx, pluginexec.NewBash(), nil)
	require.NoError(t, err)

	assertOut := func(res *engine.ExecuteResultLocks) {
		fs := hfstest.New(t)
		err = hartifact.Unpack(ctx, res.FindOutputs("")[0].Artifact, fs)
		require.NoError(t, err)

		b, err := hfs.ReadFile(fs.At("out_child"))
		require.NoError(t, err)

		assert.Equal(t, "hello\n", string(b))
	}

	{ // This will run all
		rs, clean := e.NewRequestState()
		defer clean()

		res, err := e.Result(ctx, rs, "", "child", []string{engine.AllOutputs})
		require.NoError(t, err)
		defer res.Unlock(ctx)

		assertOut(res)
		res.Unlock(ctx)
	}

	{ // this should reuse cache from deps
		rs, clean := e.NewRequestState()
		defer clean()

		res, err := e.Result(ctx, rs, "", "child", []string{engine.AllOutputs})
		require.NoError(t, err)
		defer res.Unlock(ctx)

		assertOut(res)
		res.Unlock(ctx)
	}
}
