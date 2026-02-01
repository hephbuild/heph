package enginee2e

import (
	"testing"

	"github.com/hephbuild/heph/internal/htypes"
	"github.com/hephbuild/heph/lib/tref"

	"github.com/go-faker/faker/v4"
	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/pluginexec"
	"github.com/hephbuild/heph/plugin/plugingroup"
	"github.com/hephbuild/heph/plugin/pluginstaticprovider"
	"github.com/stretchr/testify/require"
	"google.golang.org/protobuf/types/known/structpb"
)

func TestGroup(t *testing.T) {
	ctx := t.Context()

	dir := t.TempDir()

	e, err := engine.New(ctx, dir, engine.Config{})
	require.NoError(t, err)

	pkg := faker.UUIDDigit()

	staticprovider := pluginstaticprovider.New([]pluginstaticprovider.Target{
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(pkg),
					Name:    htypes.Ptr("t1"),
				}.Build(),
				Driver: htypes.Ptr("exec"),
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{"sh", "-c", "-e", `echo hello > $OUT`}),
					"out": hstructpb.NewStringsValue([]string{"out1"}),
				},
			}.Build(),
		},
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(pkg),
					Name:    htypes.Ptr("t2"),
				}.Build(),
				Driver: htypes.Ptr("exec"),
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{"sh", "-c", "-e", `echo world > $OUT`}),
					"out": hstructpb.NewStringsValue([]string{"out2"}),
				},
			}.Build(),
		},
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(pkg),
					Name:    htypes.Ptr("g"),
				}.Build(),
				Driver: htypes.Ptr("group"),
				Config: map[string]*structpb.Value{
					"deps": hstructpb.NewStringsValue([]string{
						tref.Format(pluginv1.TargetRef_builder{
							Package: htypes.Ptr(pkg),
							Name:    htypes.Ptr("t1"),
						}.Build()),
						tref.Format(pluginv1.TargetRef_builder{
							Package: htypes.Ptr(pkg),
							Name:    htypes.Ptr("t2"),
						}.Build()),
					}),
				},
			}.Build(),
		},
	})

	_, err = e.RegisterProvider(ctx, staticprovider, engine.RegisterProviderConfig{})
	require.NoError(t, err)

	_, err = e.RegisterDriver(ctx, pluginexec.NewExec(), nil)
	require.NoError(t, err)

	_, err = e.RegisterDriver(ctx, plugingroup.New(), nil)
	require.NoError(t, err)

	rs, clean := e.NewRequestState()
	defer clean()

	res, err := e.Result(ctx, rs, pkg, "g", []string{""})
	require.NoError(t, err)
	defer res.Unlock(ctx)

	require.Len(t, res.Artifacts, 2)
}

func TestGroupNamed(t *testing.T) {
	ctx := t.Context()

	dir := t.TempDir()

	e, err := engine.New(ctx, dir, engine.Config{})
	require.NoError(t, err)

	pkg := faker.UUIDDigit()

	staticprovider := pluginstaticprovider.New([]pluginstaticprovider.Target{
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(pkg),
					Name:    htypes.Ptr("t"),
				}.Build(),
				Driver: htypes.Ptr("bash"),
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{`echo hello > $OUT_1`, `echo hello > $OUT_2`}),
					"out": hstructpb.NewMapStringStringValue(map[string]string{
						"1": "out1",
						"2": "out2",
					}),
				},
			}.Build(),
		},
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(pkg),
					Name:    htypes.Ptr("g"),
				}.Build(),
				Driver: htypes.Ptr("group"),
				Config: map[string]*structpb.Value{
					"deps": hstructpb.NewStringsValue([]string{
						tref.Format(pluginv1.TargetRef_builder{
							Package: htypes.Ptr(pkg),
							Name:    htypes.Ptr("t"),
						}.Build()),
					}),
				},
			}.Build(),
		},
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(pkg),
					Name:    htypes.Ptr("res"),
				}.Build(),
				Driver: htypes.Ptr("bash"),
				Config: map[string]*structpb.Value{
					"deps": hstructpb.NewStringsValue([]string{
						tref.Format(pluginv1.TargetRef_builder{
							Package: htypes.Ptr(pkg),
							Name:    htypes.Ptr("g"),
						}.Build()),
					}),
				},
			}.Build(),
		},
	})

	_, err = e.RegisterProvider(ctx, staticprovider, engine.RegisterProviderConfig{})
	require.NoError(t, err)

	_, err = e.RegisterDriver(ctx, pluginexec.NewBash(), nil)
	require.NoError(t, err)

	_, err = e.RegisterDriver(ctx, plugingroup.New(), nil)
	require.NoError(t, err)

	rs, clean := e.NewRequestState()
	defer clean()

	res, err := e.Result(ctx, rs, pkg, "res", []string{""})
	require.NoError(t, err)
	defer res.Unlock(ctx)
}
