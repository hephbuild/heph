package enginee2e

import (
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

func TestSanityTool(t *testing.T) {
	ctx := t.Context()

	dir := t.TempDir()

	e, err := engine.New(ctx, dir, engine.Config{})
	require.NoError(t, err)

	staticprovider := pluginstaticprovider.New([]pluginstaticprovider.Target{
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "tools",
					Name:    "mytool",
				},
				Driver: "bash",
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{
						`echo '#!/usr/bin/env bash' > $OUT`,
						`echo 'echo hello' > $OUT`,
						`chmod +x $OUT`,
					}),
					"out": hstructpb.NewStringsValue([]string{"screw"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: "some/package",
					Name:    "sometarget",
				},
				Driver: "bash",
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{
						`which screw || echo 'screw bin not found'`,
						`screw > out`,
					}),
					"out":   hstructpb.NewStringsValue([]string{"out"}),
					"tools": hstructpb.NewStringsValue([]string{"//tools:mytool"}),
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

	res, err := e.Result(ctx, &engine.RequestState{}, "some/package", "sometarget", []string{""})
	require.NoError(t, err)
	defer res.Unlock(ctx)

	require.Len(t, res.Artifacts, 2)

	fs2 := hfstest.New(t)
	err = hartifact.Unpack(ctx, res.FindOutputs("")[0].Artifact, fs2)
	require.NoError(t, err)

	b, err := hfs.ReadFile(fs2, "some/package/out")
	require.NoError(t, err)

	assert.Equal(t, "hello\n", string(b))
}
