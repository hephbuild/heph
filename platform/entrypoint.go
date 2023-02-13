package platform

import (
	"fmt"
	log "heph/hlog"
	"io"
	"os"
	"strings"
)

type EntrypointExec interface {
	ExecArgs(ctx EntrypointContext) ([]string, error)
}

type Entrypoint interface {
	EntrypointExec
	ShellEntrypoint(initfile string) (EntrypointExec, error)
	ShellPrint(args []string) string
}

func bashArgs(so, lo []string) []string {
	// Bash also interprets a number of multi-character options. These options must appear on the command line
	// before the single-character options to be recognized.
	return append(
		append([]string{"bash", "--noprofile"}, lo...),
		append([]string{"-o", "pipefail"}, so...)...,
	)
}

func shArgs(initfile string, so []string) []string {
	base := []string{"sh"}
	if initfile != "" {
		base = []string{"env", "ENV=" + initfile, "sh"}
	}
	return append(base, so...)
}

type EntrypointContext struct {
	Args    []string
	CmdArgs []string // args coming from the terminal
	Env     map[string]string
}

type bashInteractiveEntrypoint struct {
	initfile string
}

func (b bashInteractiveEntrypoint) ExecArgs(ctx EntrypointContext) ([]string, error) {
	var lo []string
	if b.initfile != "" {
		lo = []string{"--rcfile", b.initfile}
	}
	// Options should be passed through the initfile
	return bashArgs(
		nil,
		lo,
	), nil
}

type shInteractiveEntrypoint struct {
	initfile string
}

func (b shInteractiveEntrypoint) ExecArgs(ctx EntrypointContext) ([]string, error) {
	// Options should be passed through the initfile
	return shArgs(
		b.initfile,
		nil,
	), nil
}

func NewInteractiveEntrypoint(tmpDir, cmds string, entrypoint Entrypoint) (EntrypointExec, func(), error) {
	cmds = strings.TrimSpace(cmds)

	var initfile string
	if len(cmds) > 0 {
		f, err := os.CreateTemp(tmpDir, "")
		if err != nil {
			return nil, nil, err
		}

		content, err := RenderInitFile(cmds)
		if err != nil {
			return nil, nil, err
		}
		_, _ = io.WriteString(f, content)

		err = f.Close()
		if err != nil {
			return nil, nil, err
		}

		initfile = f.Name()
	}

	entrypointExec, err := entrypoint.ShellEntrypoint(initfile)
	if err != nil {
		return nil, nil, err
	}

	return entrypointExec, func() {
		if initfile != "" {
			_ = os.Remove(initfile)
		}
	}, nil
}

type bashEntrypoint struct{}

func (b bashEntrypoint) ShellEntrypoint(initfile string) (EntrypointExec, error) {
	return bashInteractiveEntrypoint{initfile: initfile}, nil
}

func (b bashEntrypoint) ExecArgs(ctx EntrypointContext) ([]string, error) {
	args := bashArgs(
		[]string{ /*"-x",*/ "-u", "-e", "-c", strings.Join(ctx.Args, "\n")},
		[]string{"--norc"},
	)

	if len(ctx.CmdArgs) == 0 {
		return args, nil
	} else {
		// https://unix.stackexchange.com/a/144519
		args = append(args, "bash")
		args = append(args, ctx.CmdArgs...)
		return args, nil
	}
}

func (b bashEntrypoint) ShellPrint(args []string) string {
	return strings.Join(args, "\n")
}

var BashEntrypoint Entrypoint = bashEntrypoint{}

type shEntrypoint struct{}

func (b shEntrypoint) ShellEntrypoint(initfile string) (EntrypointExec, error) {
	return shInteractiveEntrypoint{initfile: initfile}, nil
}

func (b shEntrypoint) ExecArgs(ctx EntrypointContext) ([]string, error) {
	args := shArgs(
		"",
		[]string{ /*"-x",*/ "-u", "-e", "-c", strings.Join(ctx.Args, "\n")},
	)

	if len(ctx.CmdArgs) == 0 {
		return args, nil
	} else {
		// https://unix.stackexchange.com/a/144519
		args = append(args, "sh")
		args = append(args, ctx.CmdArgs...)
		return args, nil
	}
}

func (b shEntrypoint) ShellPrint(args []string) string {
	return strings.Join(args, "\n")
}

var ShEntrypoint Entrypoint = shEntrypoint{}

type execEntrypoint struct{}

func (e execEntrypoint) ShellEntrypoint(initfile string) (EntrypointExec, error) {
	log.Warnf("This target expects to run with entrypoint exec, we are best-effort trying to use sh for shell")

	return shInteractiveEntrypoint{initfile: initfile}, nil
}

func (e execEntrypoint) ExecArgs(ctx EntrypointContext) ([]string, error) {
	for i, arg := range ctx.Args {
		if strings.HasPrefix(arg, "$") {
			v, ok := ctx.Env[strings.TrimPrefix(arg, "$")]
			if !ok {
				return nil, fmt.Errorf("%v is unbound", arg)
			}
			ctx.Args[i] = v
		}
	}

	return append(ctx.Args, ctx.CmdArgs...), nil
}

func (e execEntrypoint) ShellPrint(args []string) string {
	return strings.Join(args, " ")
}

var ExecEntrypoint Entrypoint = execEntrypoint{}
