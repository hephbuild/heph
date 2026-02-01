package enginee2e

import (
	"os/exec"
	"testing"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hfs/hfstest"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	"github.com/hephbuild/heph/internal/htypes"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/pluginbin"
	"github.com/hephbuild/heph/plugin/pluginexec"
	"github.com/hephbuild/heph/plugin/pluginnix"
	"github.com/hephbuild/heph/plugin/pluginstaticprovider"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"google.golang.org/protobuf/types/known/structpb"
)

func skipIfNoNix(t *testing.T) {
	_, err := exec.LookPath("nix-build")
	if err != nil {
		t.Skip("nix-build is not installed")
	}
}

func TestNixWrapper(t *testing.T) {
	skipIfNoNix(t)

	ctx := t.Context()
	dir := t.TempDir()

	e, err := engine.New(ctx, dir, engine.Config{})
	require.NoError(t, err)

	staticprovider := pluginstaticprovider.New([]pluginstaticprovider.Target{
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr("tools"),
					Name:    htypes.Ptr("hello_wrapper"),
				}.Build(),
				Driver: htypes.Ptr("nix-wrapper"),
				Config: map[string]*structpb.Value{
					"packages": hstructpb.NewStringsValue([]string{"hello"}),
				},
			}.Build(),
		},
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr("test"),
					Name:    htypes.Ptr("use_wrapper"),
				}.Build(),
				Driver: htypes.Ptr("bash"),
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{
						"$TOOL > $OUT",
					}),
					"out":   structpb.NewStringValue("output"),
					"tools": hstructpb.NewStringsValue([]string{"//tools:hello_wrapper"}),
				},
			}.Build(),
		},
	})

	_, err = e.RegisterProvider(ctx, staticprovider, engine.RegisterProviderConfig{})
	require.NoError(t, err)

	_, err = e.RegisterDriver(ctx, pluginexec.NewBash(), nil)
	require.NoError(t, err)
	_, err = e.RegisterDriver(ctx, pluginbin.New(), nil)
	require.NoError(t, err)
	_, err = e.RegisterDriver(ctx, pluginnix.NewWrapper(), nil)
	require.NoError(t, err)

	rs, clean := e.NewRequestState()
	defer clean()

	res, err := e.Result(ctx, rs, "test", "use_wrapper", []string{""})
	require.NoError(t, err)
	defer res.Unlock(ctx)

	require.Len(t, res.Artifacts, 1)

	fs2 := hfstest.New(t)
	err = hartifact.Unpack(ctx, res.FindOutputs("")[0].Artifact, fs2)
	require.NoError(t, err)

	b, err := hfs.ReadFile(fs2, "test/output")
	require.NoError(t, err)

	// Verify that hello was executed successfully
	assert.Contains(t, string(b), "Hello, world!")
}

func TestNixWrapperMultiProgram(t *testing.T) {
	skipIfNoNix(t)

	ctx := t.Context()
	dir := t.TempDir()

	e, err := engine.New(ctx, dir, engine.Config{})
	require.NoError(t, err)

	staticprovider := pluginstaticprovider.New([]pluginstaticprovider.Target{
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr("tools"),
					Name:    htypes.Ptr("coreutils"),
				}.Build(),
				Driver: htypes.Ptr("nix-wrapper"),
				Config: map[string]*structpb.Value{
					"packages": hstructpb.NewStringsValue([]string{"coreutils"}),
					"programs": hstructpb.NewStringsValue([]string{"echo", "cat", "ls"}),
				},
			}.Build(),
		},
		{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr("test"),
					Name:    htypes.Ptr("use_multi"),
				}.Build(),
				Driver: htypes.Ptr("bash"),
				Config: map[string]*structpb.Value{
					"run": hstructpb.NewStringsValue([]string{
						"$TOOL_ECHO 'test output' > $OUT",
						"$TOOL_LS -la ${PATH%%:*} >> $OUT",
					}),
					"out":   structpb.NewStringValue("output"),
					"tools": hstructpb.NewStringsValue([]string{"//tools:coreutils"}),
				},
			}.Build(),
		},
	})

	_, err = e.RegisterProvider(ctx, staticprovider, engine.RegisterProviderConfig{})
	require.NoError(t, err)

	_, err = e.RegisterDriver(ctx, pluginexec.NewBash(), nil)
	require.NoError(t, err)
	_, err = e.RegisterDriver(ctx, pluginbin.New(), nil)
	require.NoError(t, err)
	_, err = e.RegisterDriver(ctx, pluginnix.NewWrapper(), nil)
	require.NoError(t, err)

	rs, clean := e.NewRequestState()
	defer clean()

	res, err := e.Result(ctx, rs, "test", "use_multi", []string{""})
	require.NoError(t, err)
	defer res.Unlock(ctx)

	require.Len(t, res.Artifacts, 1)

	fs2 := hfstest.New(t)
	err = hartifact.Unpack(ctx, res.FindOutputs("")[0].Artifact, fs2)
	require.NoError(t, err)

	b, err := hfs.ReadFile(fs2, "test/output")
	require.NoError(t, err)

	output := string(b)
	// Verify that echo worked
	assert.Contains(t, output, "test output")
	// Verify that ls shows the wrapper binaries
	assert.Contains(t, output, "echo")
	assert.Contains(t, output, "cat")
	assert.Contains(t, output, "ls")
}
