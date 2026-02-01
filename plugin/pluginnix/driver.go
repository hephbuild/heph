package pluginnix

import (
	"context"
	"strings"

	"github.com/hephbuild/heph/internal/hmaps"
	"github.com/hephbuild/heph/internal/hproto/hashpb"
	"github.com/hephbuild/heph/internal/hproto/hstructpb"
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
			return wrapWithNix(t, pluginexec.InteractiveBashArgs(strings.Join(t.GetTarget().GetRun(), "\\n"), sandboxPath))
		},
		options...,
	)
}

// parseWrapperConfig extracts nix-wrapper specific parameters and builds exec config.
func parseWrapperConfig(ctx context.Context, ref *pluginv1.TargetRef, config map[string]*structpb.Value) (*pluginv1.TargetDef, error) {
	nixPackages := make([]string, 0)
	nixPrograms := make([]string, 0)

	// Extract packages list (nix-wrapper specific parameter)
	if packagesVal := config["packages"]; packagesVal != nil {
		if listVal := packagesVal.GetListValue(); listVal != nil {
			for _, val := range listVal.GetValues() {
				if strVal := val.GetStringValue(); strVal != "" {
					nixPackages = append(nixPackages, strVal)
				}
			}
		}
	}

	// Extract programs list (nix-wrapper specific parameter)
	if programsVal := config["programs"]; programsVal != nil {
		if listVal := programsVal.GetListValue(); listVal != nil {
			for _, val := range listVal.GetValues() {
				if strVal := val.GetStringValue(); strVal != "" {
					nixPrograms = append(nixPrograms, strVal)
				}
			}
		}
	}

	if len(nixPrograms) == 0 {
		nixPrograms = nixPackages
	}

	execConfig := map[string]*structpb.Value{
		"tools": hstructpb.NewStringsValue([]string{"//@heph/bin:nix-build"}), // Hardcode nix as required tool
	}

	if len(nixPrograms) == 1 {
		execConfig["out"] = structpb.NewStringValue("bin/" + nixPrograms[0])
	} else {
		out := make(map[string]string, len(nixPrograms))
		for _, program := range nixPrograms {
			out[program] = "bin/" + program
		}
		execConfig["out"] = hstructpb.NewMapStringStringValue(out)
	}

	execTarget, execTargetHash, err := pluginexec.ConfigToExecv1(ctx, ref, execConfig, nil)
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
		Programs: nixPrograms,
	}.Build(), execTargetHash)
}

func wrapperToExecArgs(sandboxPath string, t *nixv1.Target, termargs []string, interactive bool) []string {
	// The Wrapper driver generates wrapper scripts during build
	packages := t.GetPackages()
	programs := t.GetPrograms()

	// Validate packages
	if len(packages) == 0 {
		return pluginexec.BashArgs("echo 'Error: packages list cannot be empty'; exit 1", termargs)
	}

	// If programs is empty, default to packages
	if len(programs) == 0 {
		return pluginexec.BashArgs("echo 'Error: programs list cannot be empty'; exit 1", termargs)
	}

	// Generate wrapper script that builds Nix wrappers on-demand
	// The script will:
	// 1. Generate Nix expression for wrappers
	// 2. Build with nix-build
	// 3. Copy wrappers to output directory

	script := `
# Generate Nix expression
cat > wrapper.nix <<'NIXEOF'
with import <nixpkgs> {};
let
  packages = [ ` + strings.Join(packages, " ") + ` ];
  programs = [ "` + strings.Join(programs, `" "`) + `" ];
  
  makeWrapper = prog: writeTextFile {
    name = prog;
    executable = true;
    destination = "/bin/${prog}";
    text = ''
      #!/usr/bin/env sh
      export PATH=${lib.makeBinPath packages}:$PATH
      export PKG_CONFIG_PATH="${lib.makeSearchPath "lib/pkgconfig" packages}"
      export CMAKE_PREFIX_PATH="${lib.concatStringsSep ":" packages}"
      export NIX_CFLAGS_COMPILE="${lib.concatStringsSep " " (map (p: "-isystem ${p}/include") (lib.filter (p: p ? include) packages))}"
      export NIX_LDFLAGS="${lib.concatStringsSep " " (map (p: "-L ${p}/lib") (lib.filter (p: p ? lib) packages))}"
      exec ${prog} "$@"
    '';
  };
in
buildEnv {
  name = "wrappers";
  paths = map makeWrapper programs;
}
NIXEOF

# Build wrappers
WRAPPER_PATH=$(nix-build wrapper.nix --no-out-link)

OUT="bin"

# Copy wrappers to output
mkdir -p $OUT
cp -r $WRAPPER_PATH/bin/* $OUT/
chmod +x $OUT/*

echo "Generated wrappers for: ` + strings.Join(programs, ", ") + `"
ls -la $OUT/
`

	if interactive {
		return pluginexec.InteractiveBashArgs(script, sandboxPath)
	}

	return pluginexec.BashArgs(script, termargs)
}

const NameWrapper = "nix-wrapper"

func NewWrapper(options ...Option) *Plugin {
	return pluginexec.New[*nixv1.Target](
		NameWrapper,
		(*nixv1.Target).GetTarget,
		parseWrapperConfig,
		applyTransitive,
		func(sandboxPath string, t *nixv1.Target, termargs []string) []string {
			return wrapperToExecArgs(sandboxPath, t, termargs, false)
		},
		options...,
	)
}

const NameShellWrapper = "nix-wrapper@shell"

func NewShellWrapper(options ...Option) *Plugin {
	return pluginexec.New[*nixv1.Target](
		NameShellWrapper,
		(*nixv1.Target).GetTarget,
		parseWrapperConfig,
		applyTransitive,
		func(sandboxPath string, t *nixv1.Target, termargs []string) []string {
			return wrapperToExecArgs(sandboxPath, t, termargs, true)
		},
		options...,
	)
}
