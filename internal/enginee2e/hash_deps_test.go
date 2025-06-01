package enginee2e

import (
	"context"
	"testing"
	"time"

	"github.com/hephbuild/heph/internal/hproto/hstructpb"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hartifact"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/pluginexec"
	"github.com/hephbuild/heph/plugin/pluginstaticprovider"
	"github.com/stretchr/testify/require"
	"google.golang.org/protobuf/types/known/structpb"
)

func TestHashDeps(t *testing.T) {
	ctx := context.Background()

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
		res, err := e.Result(ctx, "some/package", "sometarget", []string{""}, &engine.RequestState{})
		require.NoError(t, err)
		defer res.Unlock(ctx)

		require.Len(t, res.Artifacts, 2)

		m, err := hartifact.ManifestFromArtifact(ctx, res.FindManifest().Artifact)
		require.NoError(t, err)

		at = m.CreatedAt
		res.Unlock(ctx)
	}

	{
		res, err := e.Result(ctx, "some/package", "sometarget", []string{""}, &engine.RequestState{})
		require.NoError(t, err)
		defer res.Unlock(ctx)

		require.Len(t, res.Artifacts, 2)

		m, err := hartifact.ManifestFromArtifact(ctx, res.FindManifest().Artifact)
		require.NoError(t, err)

		require.Equal(t, at, m.CreatedAt)
		res.Unlock(ctx)
	}
}
