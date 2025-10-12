package pluginnix

import (
	"context"
	"strings"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/pluginexec"
	execv1 "github.com/hephbuild/heph/plugin/pluginexec/gen/heph/plugin/exec/v1"
	nixv1 "github.com/hephbuild/heph/plugin/pluginnix/gen/heph/plugin/nix/v1"
	"google.golang.org/protobuf/types/known/structpb"
)

func wrapWithNix(t *nixv1.Target, args []string) []string {
	nargs := make([]string, 0, len(args))
	nargs = append(nargs, "nix")
	nargs = append(nargs, "shell")
	if len(t.GetPackages()) > 0 {
		for _, p := range t.GetPackages() {
			nargs = append(nargs, "nixpkgs#"+p)
		}
	}
	nargs = append(nargs, "-c")
	nargs = append(nargs, args...)

	return nargs
}

const NameBash = "nix-bash"

type Option = pluginexec.Option[*nixv1.Target]
type Plugin = pluginexec.Plugin[*nixv1.Target]

const nixConfig = `
experimental-features = nix-command flakes
`

func ParseConfig(ctx context.Context, ref *pluginv1.TargetRef, config map[string]*structpb.Value) (*pluginv1.TargetDef, error) {
	nixPackages := make([]string, 0)

	return pluginexec.ParseConfig(ctx, ref, config, func(spec pluginexec.Spec, target *execv1.Target) (*nixv1.Target, error) {
		renv := target.GetRuntimeEnv()
		if renv == nil {
			renv = map[string]string{}
		}
		renv["NIX_CONFIG"] = nixConfig
		target.SetRuntimeEnv(renv)

		return nixv1.Target_builder{
			Target:   target,
			Packages: nixPackages,
		}.Build(), nil
	}, func(ref *pluginv1.TargetRefWithOutput) bool {
		if ref.GetPackage() != "@nix" {
			return true
		}

		nixPackages = append(nixPackages, ref.GetName())

		return false
	})
}

func NewBash(options ...Option) *Plugin {
	return pluginexec.New[*nixv1.Target](
		NameBash,
		func(t *nixv1.Target) *execv1.Target { return t.GetTarget() },
		ParseConfig,
		func(sandboxPath string, t *nixv1.Target, termargs []string) []string {
			return wrapWithNix(t, pluginexec.BashArgs(strings.Join(t.GetTarget().GetRun(), "\n"), termargs))
		},
		options...,
	)
}

const NameBashShell = NameBash + "@shell"

func NewInteractiveBash(options ...Option) *Plugin {
	return pluginexec.New[*nixv1.Target](
		NameBashShell,
		func(t *nixv1.Target) *execv1.Target { return t.GetTarget() },
		ParseConfig,
		func(sandboxPath string, t *nixv1.Target, termargs []string) []string {
			return wrapWithNix(t, pluginexec.InteractiveBashArgs(strings.Join(t.GetTarget().GetRun(), "\n"), sandboxPath))
		},
		options...,
	)
}
