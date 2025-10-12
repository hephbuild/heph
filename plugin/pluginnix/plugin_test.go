package pluginnix

import (
	"os"
	"os/exec"
	"testing"

	"github.com/hephbuild/heph/internal/hproto/hstructpb"
	"github.com/hephbuild/heph/internal/htypes"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"google.golang.org/protobuf/encoding/protojson"
	"google.golang.org/protobuf/types/known/structpb"
)

func skipIfNoNix(t *testing.T) {
	_, err := exec.LookPath("nix-shell")

	if err != nil {
		t.Skip("nix-shell is not installed")
	}
}

func TestSanity(t *testing.T) {
	skipIfNoNix(t)

	ctx := t.Context()
	sandboxPath := t.TempDir()
	defer os.RemoveAll(sandboxPath)

	p := NewBash()

	{
		res, err := p.Config(ctx, &pluginv1.ConfigRequest{})
		require.NoError(t, err)

		b, err := protojson.Marshal(res.GetTargetSchema())
		require.NoError(t, err)
		require.NotEmpty(t, b)
		// require.JSONEq(t, `{"name":"Target", "field":[{"name":"run", "number":1, "label":"LABEL_REPEATED", "type":"TYPE_STRING", "jsonName":"run"}]}`, string(b))
	}

	var def *pluginv1.TargetDef
	{
		runArg, err := structpb.NewValue([]any{"echo hello"})
		require.NoError(t, err)

		res, err := p.Parse(ctx, pluginv1.ParseRequest_builder{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr("some/pkg"),
					Name:    htypes.Ptr("target"),
				}.Build(),
				Config: map[string]*structpb.Value{
					"run": runArg,
				},
			}.Build(),
		}.Build())
		require.NoError(t, err)

		def = res.GetTarget()
	}

	{
		res, err := p.Run(ctx, pluginv1.RunRequest_builder{
			Target:      def,
			SandboxPath: htypes.Ptr(sandboxPath),
		}.Build())
		require.NoError(t, err)

		assert.Len(t, res.GetArtifacts(), 1)
	}
}

func TestToolSanity(t *testing.T) {
	skipIfNoNix(t)

	ctx := t.Context()
	sandboxPath := t.TempDir()
	defer os.RemoveAll(sandboxPath)

	p := NewBash()

	{
		_, err := p.Config(ctx, &pluginv1.ConfigRequest{})
		require.NoError(t, err)
	}

	var def *pluginv1.TargetDef
	{
		res, err := p.Parse(ctx, pluginv1.ParseRequest_builder{
			Spec: pluginv1.TargetSpec_builder{
				Ref: pluginv1.TargetRef_builder{
					Package: htypes.Ptr("some/pkg"),
					Name:    htypes.Ptr("target"),
				}.Build(),
				Config: map[string]*structpb.Value{
					"run":   hstructpb.NewStringsValue([]string{"cowsay hello"}),
					"tools": hstructpb.NewStringsValue([]string{"//@nix:cowsay"}),
				},
			}.Build(),
		}.Build())
		require.NoError(t, err)

		def = res.GetTarget()
	}

	{
		res, err := p.Run(ctx, pluginv1.RunRequest_builder{
			Target:      def,
			SandboxPath: htypes.Ptr(sandboxPath),
		}.Build())
		require.NoError(t, err)

		assert.Len(t, res.GetArtifacts(), 1)
	}
}
