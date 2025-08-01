package enginee2e

import (
	"io"
	"testing"

	"github.com/hephbuild/heph/internal/htypes"

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

func newValueMust(v any) *structpb.Value {
	pv, err := structpb.NewValue(v)
	if err != nil {
		panic(err)
	}

	return pv
}

func TestSanity(t *testing.T) {
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
					Name:    htypes.Ptr("sometarget"),
				}.Build(),
				Driver: htypes.Ptr("exec"),
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{"sh", "-c", "-e", `echo hello > out`}),
					"out": hstructpb.NewStringsValue([]string{"out"}),
				},
			}.Build(),
		},
	})

	_, err = e.RegisterProvider(ctx, staticprovider)
	require.NoError(t, err)

	execdriver := pluginexec.New()
	_, err = e.RegisterDriver(ctx, execdriver, nil)
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

	assert.Equal(t, "hello\n", string(b))
}

func TestSanity2(t *testing.T) {
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
					Name:    htypes.Ptr("a"),
				}.Build(),
				Driver: htypes.Ptr("sh"),
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{`echo hello > $OUT`}),
					"out": structpb.NewStringValue("outa"),
				},
			}.Build(),
		},
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(pkg),
					Name:    htypes.Ptr("b"),
				}.Build(),
				Driver: htypes.Ptr("sh"),
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{`echo world > $OUT`}),
					"out": structpb.NewStringValue("outb"),
				},
			}.Build(),
		},
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(pkg),
					Name:    htypes.Ptr("c"),
				}.Build(),
				Driver: htypes.Ptr("sh"),
				Config: map[string]*structpb.Value{
					"deps": hstructpb.NewMapStringStringValue(map[string]string{
						"d1": "//" + pkg + ":a",
						"d2": "//" + pkg + ":b",
					}),
					"run": hstructpb.NewStringsValue([]string{`cat $SRC_D1 $SRC_D2 > $OUT`}),
					"out": structpb.NewStringValue("out"),
				},
			}.Build(),
		},
	})

	_, err = e.RegisterProvider(ctx, staticprovider)
	require.NoError(t, err)

	execdriver := pluginexec.NewSh()
	_, err = e.RegisterDriver(ctx, execdriver, nil)
	require.NoError(t, err)

	rs, clean := e.NewRequestState()
	defer clean()

	res, err := e.Result(ctx, rs, pkg, "c", []string{""})
	require.NoError(t, err)
	defer res.Unlock(ctx)

	require.Len(t, res.Artifacts, 1)

	r, err := hartifact.FileReader(ctx, res.FindOutputs("")[0].Artifact)
	require.NoError(t, err)
	defer r.Close()

	b, err := io.ReadAll(r)
	require.NoError(t, err)

	assert.Equal(t, "hello\nworld\n", string(b))
}
