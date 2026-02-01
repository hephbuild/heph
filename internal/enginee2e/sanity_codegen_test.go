package enginee2e

import (
	"fmt"
	"io"
	"strings"
	"testing"

	"github.com/hephbuild/heph/internal/htypes"
	"github.com/hephbuild/heph/internal/tmatch"
	"github.com/hephbuild/heph/lib/tref"
	"github.com/hephbuild/heph/plugin/pluginfs"

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

func TestCodegen(t *testing.T) {
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
					Name:    htypes.Ptr("gen"),
				}.Build(),
				Driver: htypes.Ptr("exec"),
				Config: map[string]*structpb.Value{
					"run":     hstructpb.NewStringsValue([]string{"sh", "-c", "-e", `echo hello > $OUT`}),
					"out":     hstructpb.NewStringsValue([]string{"gen"}),
					"codegen": structpb.NewStringValue("copy"),
				},
			}.Build(),
		},
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(pkg),
					Name:    htypes.Ptr("t1"),
				}.Build(),
				Driver: htypes.Ptr("exec"),
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{"sh", "-c", "-e", `echo hello > $OUT`}),
					"out": hstructpb.NewStringsValue([]string{"out_t1"}),
				},
			}.Build(),
		},
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr(pkg),
					Name:    htypes.Ptr("ls"),
				}.Build(),
				Driver: htypes.Ptr("exec"),
				Config: map[string]*structpb.Value{
					"deps":  hstructpb.NewStringsValue([]string{tref.FormatGlob(pkg, "*", nil), tref.Format(tref.New(pkg, "t1", nil))}),
					"run":   hstructpb.NewStringsValue([]string{"sh", "-c", "-e", `RES=$(ls); echo $RES > $OUT`}),
					"out":   hstructpb.NewStringsValue([]string{"out_ls"}),
					"cache": structpb.NewBoolValue(false),
				},
			}.Build(),
		},
	})

	_, err = e.RegisterProvider(ctx, staticprovider, engine.RegisterProviderConfig{})
	require.NoError(t, err)

	_, err = e.RegisterProvider(ctx, pluginfs.NewProvider(), engine.RegisterProviderConfig{})
	require.NoError(t, err)

	_, err = e.RegisterDriver(ctx, pluginfs.NewDriver(), nil)
	require.NoError(t, err)

	execdriver := pluginexec.NewExec()
	_, err = e.RegisterDriver(ctx, execdriver, nil)
	require.NoError(t, err)

	assertEmpty := func(t *testing.T) {
		t.Helper()

		rs, clean := e.NewRequestState()
		defer clean()

		res, err := e.Result(ctx, rs, pkg, "ls", []string{""})
		require.NoError(t, err)
		defer res.Unlock(ctx)

		require.Len(t, res.Artifacts, 1)

		r, err := hartifact.FileReader(ctx, res.FindOutputs("")[0].Artifact)
		require.NoError(t, err)
		defer r.Close()

		b, err := io.ReadAll(r)
		require.NoError(t, err)

		assert.Equal(t, "out_t1", strings.TrimSpace(string(b)))
	}

	assertEmpty(t)

	{
		rs, clean := e.NewRequestState()
		defer clean()

		res, err := e.Result(ctx, rs, pkg, "gen", []string{""})
		require.NoError(t, err)
		res.Unlock(ctx)
	}

	assertEmpty(t)
}

func TestCodegenValidateConflict1(t *testing.T) {
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
					"run":     hstructpb.NewStringsValue([]string{"sh", "-c", "-e", `echo hello > $OUT`}),
					"out":     hstructpb.NewStringsValue([]string{"dir/file"}),
					"codegen": structpb.NewStringValue("copy"),
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
					"run":     hstructpb.NewStringsValue([]string{"sh", "-c", "-e", `echo hello > $OUT`}),
					"out":     hstructpb.NewStringsValue([]string{"dir/file"}),
					"codegen": structpb.NewStringValue("copy"),
				},
			}.Build(),
		},
	})

	_, err = e.RegisterProvider(ctx, staticprovider, engine.RegisterProviderConfig{})
	require.NoError(t, err)

	_, err = e.RegisterProvider(ctx, pluginfs.NewProvider(), engine.RegisterProviderConfig{})
	require.NoError(t, err)

	_, err = e.RegisterDriver(ctx, pluginfs.NewDriver(), nil)
	require.NoError(t, err)

	execdriver := pluginexec.NewExec()
	_, err = e.RegisterDriver(ctx, execdriver, nil)
	require.NoError(t, err)

	rs, clean := e.NewRequestState()
	defer clean()

	err = e.Validate(ctx, rs, tmatch.All())
	require.Error(t, err)
	if !strings.Contains(err.Error(), fmt.Sprintf("//%[1]v:t1: //%[1]v:t2 already outputs %[1]v/dir/file", pkg)) &&
		!strings.Contains(err.Error(), fmt.Sprintf("//%[1]v:t2: //%[1]v:t1 already outputs %[1]v/dir/file", pkg)) {
		require.Fail(t, fmt.Sprintf("error doesnt match: %v", err.Error()))
	}
}

func TestCodegenValidateConflict2(t *testing.T) {
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
					"run":     hstructpb.NewStringsValue([]string{"sh", "-c", "-e", `echo hello > $OUT/1_file`}),
					"out":     hstructpb.NewStringsValue([]string{"dir/"}),
					"codegen": structpb.NewStringValue("copy"),
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
					"run":     hstructpb.NewStringsValue([]string{"sh", "-c", "-e", `echo hello > $OUT`}),
					"out":     hstructpb.NewStringsValue([]string{"dir/2_file"}),
					"codegen": structpb.NewStringValue("copy"),
				},
			}.Build(),
		},
	})

	_, err = e.RegisterProvider(ctx, staticprovider, engine.RegisterProviderConfig{})
	require.NoError(t, err)

	_, err = e.RegisterProvider(ctx, pluginfs.NewProvider(), engine.RegisterProviderConfig{})
	require.NoError(t, err)

	_, err = e.RegisterDriver(ctx, pluginfs.NewDriver(), nil)
	require.NoError(t, err)

	execdriver := pluginexec.NewExec()
	_, err = e.RegisterDriver(ctx, execdriver, nil)
	require.NoError(t, err)

	rs, clean := e.NewRequestState()
	defer clean()

	err = e.Validate(ctx, rs, tmatch.All())
	require.Error(t, err)
	if !strings.Contains(err.Error(), fmt.Sprintf("//%[1]v:t1: //%[1]v:t2 already outputs %[1]v/dir/2_file, it would shadow be shadowed by %[1]v/dir", pkg)) &&
		!strings.Contains(err.Error(), fmt.Sprintf("//%[1]v:t2: //%[1]v:t1 already outputs %[1]v/dir, cannot output %[1]v/dir/2_file because it's in a generated directory", pkg)) {
		require.Fail(t, fmt.Sprintf("error doesnt match: %v", err.Error()))
	}
}
