package enginee2e

import (
	"testing"
	"time"

	"github.com/hephbuild/heph/lib/tref"

	"github.com/hephbuild/heph/internal/hproto/hstructpb"

	"github.com/hephbuild/heph/internal/engine"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/pluginexec"
	"github.com/hephbuild/heph/plugin/pluginstaticprovider"
	"github.com/stretchr/testify/require"
	"google.golang.org/protobuf/types/known/structpb"
)

func TestHashDeps(t *testing.T) {
	ctx := t.Context()

	dir := t.TempDir()

	e, err := engine.New(ctx, dir, engine.Config{})
	require.NoError(t, err)

	staticprovider := pluginstaticprovider.NewFunc(func() []pluginstaticprovider.Target {
		return []pluginstaticprovider.Target{
			{
				Spec: &pluginv1.TargetSpec{
					Ref: &pluginv1.TargetRef{
						Package: "some/package",
						Name:    "sometarget",
					},
					Driver: "sh",
					Config: map[string]*structpb.Value{
						"run": hstructpb.NewStringsValue([]string{`echo hello > out`}),
						"out": hstructpb.NewStringsValue([]string{"out"}),
						"runtime_env": newValueMust(map[string]any{
							"VAR1": time.Now().String(),
						}),
					},
				},
			},
		}
	})

	_, err = e.RegisterProvider(ctx, staticprovider)
	require.NoError(t, err)

	_, err = e.RegisterDriver(ctx, pluginexec.NewSh(), nil)
	require.NoError(t, err)

	var at time.Time
	{
		rs, clean := e.NewRequestState()
		defer clean()

		res, err := e.Result(ctx, rs, "some/package", "sometarget", []string{""})
		require.NoError(t, err)
		defer res.Unlock(ctx)

		require.Len(t, res.Artifacts, 1)

		m, err := e.ResultMetaFromRef(ctx, rs, &tref.Ref{Package: "some/package", Name: "sometarget"}, nil)
		require.NoError(t, err)

		at = m.CreatedAt
		res.Unlock(ctx)
	}

	{
		rs, clean := e.NewRequestState()
		defer clean()

		res, err := e.Result(ctx, rs, "some/package", "sometarget", []string{""})
		require.NoError(t, err)
		defer res.Unlock(ctx)

		require.Len(t, res.Artifacts, 1)

		m, err := e.ResultMetaFromRef(ctx, rs, &tref.Ref{Package: "some/package", Name: "sometarget"}, nil)
		require.NoError(t, err)

		require.Equal(t, at, m.CreatedAt)
		res.Unlock(ctx)
	}
}
