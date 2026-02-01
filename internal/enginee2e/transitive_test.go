package enginee2e

import (
	"io"
	"testing"

	"github.com/hephbuild/heph/internal/htypes"
	"github.com/hephbuild/heph/lib/tref"
	"github.com/hephbuild/heph/plugin/plugingroup"

	"github.com/go-faker/faker/v4"

	"github.com/hephbuild/heph/internal/hproto/hstructpb"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hartifact"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/pluginexec"
	"github.com/hephbuild/heph/plugin/pluginstaticprovider"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"google.golang.org/protobuf/types/known/structpb"
)

func TestTransitive(t *testing.T) {
	ctx := t.Context()

	dir := t.TempDir()

	e, err := engine.New(ctx, dir, engine.Config{})
	require.NoError(t, err)

	pkg := faker.UUIDDigit()

	passEnvKey := "WRAPPER_PASS_" + pkg
	t.Setenv(passEnvKey, "1")

	staticprovider := pluginstaticprovider.New([]pluginstaticprovider.Target{
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(pkg),
					Name:    htypes.Ptr("tool_support"),
				}.Build(),
				Driver: htypes.Ptr("bash"),
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{`touch $OUT`}),
					"out": hstructpb.NewStringsValue([]string{"tool_support"}),
				},
			}.Build(),
		},
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(pkg),
					Name:    htypes.Ptr("tool"),
				}.Build(),
				Driver: htypes.Ptr("bash"),
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{`touch $OUT`}),
					"out": hstructpb.NewStringsValue([]string{"tool"}),
				},
				Transitive: pluginv1.Sandbox_builder{
					Tools: []*pluginv1.Sandbox_Tool{
						pluginv1.Sandbox_Tool_builder{
							Ref:  tref.WithOut(tref.New(pkg, "tool_support", nil), ""),
							Hash: htypes.Ptr(true),
						}.Build(),
					},
				}.Build(),
			}.Build(),
		},
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(pkg),
					Name:    htypes.Ptr("tool_wrapper"),
				}.Build(),
				Driver: htypes.Ptr("group"),
				Config: map[string]*structpb.Value{
					"deps": structpb.NewStringValue(tref.Format(tref.New(pkg, "tool", nil))),
				},
				Transitive: pluginv1.Sandbox_builder{
					Env: map[string]*pluginv1.Sandbox_Env{
						"WRAPPER": pluginv1.Sandbox_Env_builder{
							Literal: htypes.Ptr("1"),
							Hash:    htypes.Ptr(true),
						}.Build(),
						passEnvKey: pluginv1.Sandbox_Env_builder{
							Pass: htypes.Ptr(true),
							Hash: htypes.Ptr(true),
						}.Build(),
					},
				}.Build(),
			}.Build(),
		},
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(pkg),
					Name:    htypes.Ptr("sometarget"),
				}.Build(),
				Driver: htypes.Ptr("bash"),
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{
						`command -v tool >/dev/null 2>&1 && echo tool >> $OUT || exit 1`,
						`command -v tool_support >/dev/null 2>&1 && echo tool_support >> $OUT || exit 1`,
						`echo $WRAPPER >> $OUT`,
						`echo $` + passEnvKey + ` >> $OUT`,
					}),
					"tools": structpb.NewStringValue(tref.Format(tref.New(pkg, "tool_wrapper", nil))),
					"out":   hstructpb.NewStringsValue([]string{"out"}),
				},
			}.Build(),
		},
	})

	_, err = e.RegisterProvider(ctx, staticprovider, engine.RegisterProviderConfig{})
	require.NoError(t, err)

	_, err = e.RegisterDriver(ctx, plugingroup.New(), nil)
	require.NoError(t, err)

	_, err = e.RegisterDriver(ctx, pluginexec.NewBash(), nil)
	require.NoError(t, err)

	rs, clean := e.NewRequestState()
	defer clean()

	res, err := e.Result(ctx, rs, pkg, "sometarget", []string{""})
	require.NoError(t, err)
	defer res.Unlock(ctx)

	require.Len(t, res.Artifacts, 1)

	r, err := hartifact.FileReader(ctx, res.FindOutputs("")[0].Artifact)
	require.NoError(t, err)
	defer r.Close()

	b, err := io.ReadAll(r)
	require.NoError(t, err)

	assert.Equal(t, "tool\ntool_support\n1\n1\n", string(b))
}
