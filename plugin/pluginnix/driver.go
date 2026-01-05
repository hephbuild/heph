package pluginnix

import (
	"context"
	"strings"

	"github.com/hephbuild/heph/internal/hmaps"
	"github.com/hephbuild/heph/internal/hproto/hashpb"
	"github.com/hephbuild/heph/internal/htypes"
	"github.com/hephbuild/heph/lib/tref"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/pluginexec"
	execv1 "github.com/hephbuild/heph/plugin/pluginexec/gen/heph/plugin/exec/v1"
	nixv1 "github.com/hephbuild/heph/plugin/pluginnix/gen/heph/plugin/nix/v1"
	"github.com/zeebo/xxh3"
	"google.golang.org/protobuf/types/known/structpb"
)

func wrapWithNix(t *nixv1.Target, args []string) []string {
	nargs := make([]string, 0, len(args))
	nargs = append(nargs, "nix")
	nargs = append(nargs, "shell")
	nargs = append(nargs, "nixpkgs#bash")
	for _, p := range t.GetPackages() {
		nargs = append(nargs, "nixpkgs#"+p)
	}
	nargs = append(nargs, "-c")
	nargs = append(nargs, args...)

	return nargs
}

type Option = pluginexec.Option[*nixv1.Target]
type Plugin = pluginexec.Plugin[*nixv1.Target]

const nixConfig = `
experimental-features = nix-command flakes
`

func parseConfig(ctx context.Context, ref *pluginv1.TargetRef, config map[string]*structpb.Value) (*pluginv1.TargetDef, error) {
	nixPackages := make([]string, 0)

	execTarget, execTargetHash, err := pluginexec.ConfigToExecv1(ctx, ref, config, func(ref *pluginv1.TargetRefWithOutput) bool {
		if ref.GetTarget().GetPackage() != "@nix" {
			return true
		}

		nixPackages = append(nixPackages, ref.GetTarget().GetName())

		return false
	})
	if err != nil {
		return nil, err
	}

	renv := execTarget.GetEnv()
	if renv == nil {
		renv = map[string]*execv1.Target_Env{}
	}
	renv["NIX_CONFIG"] = execv1.Target_Env_builder{
		Literal: htypes.Ptr(nixConfig),
		Hash:    htypes.Ptr(true),
	}.Build()
	execTarget.SetEnv(renv)

	return toDef(ref, nixv1.Target_builder{
		Target:   execTarget,
		Packages: nixPackages,
	}.Build(), execTargetHash)
}

var omitHashPb = hmaps.Concat(map[string]struct{}{
	string((&nixv1.Target{}).ProtoReflect().Descriptor().Name()) + ".target": {},
}, tref.OmitHashPb)

func hashTarget(nixTarget *nixv1.Target, execTargetHash []byte) []byte {
	hash := xxh3.New()
	_, _ = hash.Write(execTargetHash)
	hashpb.Hash(hash, nixTarget, omitHashPb)

	return hash.Sum(nil)
}

func applyTransitive(ctx context.Context, ref *pluginv1.TargetRef, sandbox *pluginv1.Sandbox, target *nixv1.Target) (*pluginv1.TargetDef, error) {
	// TODO: filter tools
	nixPackages := make([]string, 0)

	execTarget, hash, err := pluginexec.ApplyTransitiveExecv1(ref, sandbox, target.GetTarget())
	if err != nil {
		return nil, err
	}
	target.SetTarget(execTarget)

	target.SetPackages(append(target.GetPackages(), nixPackages...))

	return toDef(ref, target, hash)
}

func toDef(ref *pluginv1.TargetRef, target *nixv1.Target, hash []byte) (*pluginv1.TargetDef, error) {
	return pluginexec.ToDef(ref, target, (*nixv1.Target).GetTarget, hashTarget(target, hash))
}

const NameBash = "nix-bash"

func NewBash(options ...Option) *Plugin {
	return pluginexec.New[*nixv1.Target](
		NameBash,
		(*nixv1.Target).GetTarget,
		parseConfig,
		applyTransitive,
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
		(*nixv1.Target).GetTarget,
		parseConfig,
		applyTransitive,
		func(sandboxPath string, t *nixv1.Target, termargs []string) []string {
			return wrapWithNix(t, pluginexec.InteractiveBashArgs(strings.Join(t.GetTarget().GetRun(), "\n"), sandboxPath))
		},
		options...,
	)
}
