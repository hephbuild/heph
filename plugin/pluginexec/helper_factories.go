package pluginexec

import (
	"context"
	"os"
	"path/filepath"
	"strings"

	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	execv1 "github.com/hephbuild/heph/plugin/pluginexec/gen/heph/plugin/exec/v1"
	"google.golang.org/protobuf/types/known/structpb"
)

func NewExec(options ...Option[*execv1.Target]) *Plugin[*execv1.Target] {
	return New[*execv1.Target](
		NameExec,
		func(t *execv1.Target) *execv1.Target { return t },
		func(ctx context.Context, ref *pluginv1.TargetRef, config map[string]*structpb.Value) (*pluginv1.TargetDef, error) {
			execTarget, execTargetHash, err := ConfigToExecv1(ctx, ref, config, nil)
			if err != nil {
				return nil, err
			}

			return execv1ToDef(ref, execTarget, execTarget, execTargetHash)
		},
		func(ctx context.Context, ref *pluginv1.TargetRef, sandbox *pluginv1.Sandbox, target *execv1.Target) (*pluginv1.TargetDef, error) {
			target, hash, err := ApplyTransitiveExecv1(ref, sandbox, target)
			if err != nil {
				return nil, err
			}

			return execv1ToDef(ref, target, target, hash)
		},
		func(sandboxPath string, t *execv1.Target, termargs []string) []string {
			return append(t.GetRun(), termargs...)
		},
		options...,
	)
}

func bashArgs(so, lo []string) []string {
	// Bash also interprets a number of multi-character options. These options must appear on the command line
	// before the single-character options to be recognized.
	return append(
		append([]string{"bash", "--noprofile"}, lo...),
		append([]string{"-o", "pipefail"}, so...)...,
	)
}

func BashArgs(cmd string, termargs []string) []string {
	args := bashArgs(
		[]string{ /*"-x",*/ "-u", "-e", "-c", cmd},
		[]string{"--norc"},
	)

	if len(termargs) == 0 {
		return args
	} else {
		// https://unix.stackexchange.com/a/144519
		args = append(args, "bash")
		args = append(args, termargs...)
		return args
	}
}

const NameBash = "bash"

func NewBash(options ...Option[*execv1.Target]) *Plugin[*execv1.Target] {
	options = append(options, WithRunToExecArgs[*execv1.Target](func(sandboxPath string, t *execv1.Target, termargs []string) []string {
		return BashArgs(strings.Join(t.GetRun(), "\n"), termargs)
	}), WithName[*execv1.Target](NameBash))

	return NewExec(options...)
}

func InteractiveBashArgs(cmd, sandboxPath string) []string {
	content, err := RenderInitFile(cmd)
	if err != nil { //nolint:staticcheck
		// TODO: log
	}

	initfilePath := filepath.Join(sandboxPath, "init.sh")

	err = os.WriteFile(initfilePath, []byte(content), 0644) //nolint:gosec
	if err != nil {                                         //nolint:staticcheck
		// TODO: log
	}

	return bashArgs(
		nil,
		[]string{"--rcfile", initfilePath},
	)
}

const NameBashShell = NameBash + "@shell"

func NewInteractiveBash(options ...Option[*execv1.Target]) *Plugin[*execv1.Target] {
	options = append(options, WithRunToExecArgs[*execv1.Target](func(sandboxPath string, t *execv1.Target, termargs []string) []string {
		return InteractiveBashArgs(strings.Join(t.GetRun(), "\n"), sandboxPath)
	}), WithName[*execv1.Target](NameBashShell))

	return NewExec(options...)
}

func shArgs(initfile string, so []string) []string {
	base := []string{"sh"}
	if initfile != "" {
		base = []string{"env", "ENV=" + initfile, "sh"}
	}
	return append(base, so...)
}

const NameSh = "sh"

func NewSh(options ...Option[*execv1.Target]) *Plugin[*execv1.Target] {
	options = append(options, WithRunToExecArgs[*execv1.Target](func(sandboxPath string, t *execv1.Target, termargs []string) []string {
		args := shArgs(
			"",
			[]string{ /*"-x",*/ "-u", "-e", "-c", strings.Join(t.GetRun(), "\n")},
		)

		if len(termargs) == 0 {
			return args
		} else {
			// https://unix.stackexchange.com/a/144519
			args = append(args, "sh")
			args = append(args, termargs...)
			return args
		}
	}), WithName[*execv1.Target](NameSh))

	return NewExec(options...)
}

const NameShShell = NameSh + "@shell"

func NewInteractiveSh(options ...Option[*execv1.Target]) *Plugin[*execv1.Target] {
	options = append(options, WithRunToExecArgs[*execv1.Target](func(sandboxPath string, t *execv1.Target, termargs []string) []string {
		return InteractiveBashArgs(strings.Join(t.GetRun(), "\n"), sandboxPath)
	}), WithName[*execv1.Target](NameShShell))

	return NewExec(options...)
}
