package enginee2e

import (
	"testing"

	"github.com/go-faker/faker/v4"
	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/pluginexec"
	"github.com/hephbuild/heph/plugin/plugingroup"
	"github.com/hephbuild/heph/plugin/pluginstaticprovider"
	"github.com/hephbuild/heph/plugin/tref"
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
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: pkg,
					Name:    "t1",
				},
				Driver: "exec",
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{"sh", "-c", "-e", `echo hello > $OUT`}),
					"out": hstructpb.NewStringsValue([]string{"out1"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: pkg,
					Name:    "t2",
				},
				Driver: "exec",
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{"sh", "-c", "-e", `echo world > $OUT`}),
					"out": hstructpb.NewStringsValue([]string{"out2"}),
				},
			},
		},
		{
			Spec: &pluginv1.TargetSpec{
				Ref: &pluginv1.TargetRef{
					Package: pkg,
					Name:    "g",
				},
				Driver: "group",
				Config: map[string]*structpb.Value{
					"deps": hstructpb.NewStringsValue([]string{
						tref.Format(&pluginv1.TargetRef{
							Package: pkg,
							Name:    "t1",
						}),
						tref.Format(&pluginv1.TargetRef{
							Package: pkg,
							Name:    "t2",
						}),
					}),
				},
			},
		},
	})

	_, err = e.RegisterProvider(ctx, staticprovider)
	require.NoError(t, err)

	_, err = e.RegisterDriver(ctx, pluginexec.New(), nil)
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
