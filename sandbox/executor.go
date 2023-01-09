package sandbox

import (
	"fmt"
	"strings"
)

func bashArgs(so, lo []string) []string {
	// Bash also interprets a number of multi-character options. These options must appear on the command line
	// before the single-character options to be recognized.
	return append(
		append([]string{"bash", "--noprofile"}, lo...),
		append([]string{"-o", "pipefail"}, so...)...,
	)
}

type ExecutorContext struct {
	Args    []string
	CmdArgs []string // args coming from the terminal
	Env     map[string]string
}

func BashShellExecutor(rcfile string) (Executor, error) {
	return Executor{
		ExecArgs: func(ctx ExecutorContext) ([]string, error) {
			return bashArgs(
				nil,
				[]string{"--rcfile", rcfile},
			), nil
		},
		ShellPrint: func(args []string) string {
			panic("not implemented")
		},
	}, nil
}

var BashExecutor = Executor{
	ExecArgs: func(ctx ExecutorContext) ([]string, error) {
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
	},
	ShellPrint: func(args []string) string {
		return strings.Join(args, "\n")
	},
}

var ExecExecutor = Executor{
	ExecArgs: func(ctx ExecutorContext) ([]string, error) {
		for i, arg := range ctx.Args {
			if strings.HasPrefix(arg, "$") {
				v, ok := ctx.Env[strings.TrimPrefix(arg, "$")]
				if !ok {
					return nil, fmt.Errorf("%v is unbound", arg)
				}
				ctx.Args[i] = v
			}
		}

		path := ctx.Env["PATH"]
		p, err := LookPath(ctx.Args[0], path)
		if err != nil {
			return nil, err
		}
		ctx.Args[0] = p

		return append(ctx.Args, ctx.CmdArgs...), nil
	},
	ShellPrint: func(args []string) string {
		return strings.Join(args, " ")
	},
}

type Executor struct {
	ExecArgs   func(ctx ExecutorContext) ([]string, error)
	ShellPrint func(args []string) string
}
