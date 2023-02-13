package platform

import (
	"context"
)

func Exec(ctx context.Context, executor Executor, entrypoint Entrypoint, tmpDir string, o ExecOptions, shell bool) error {
	var entrypointExec EntrypointExec = entrypoint
	if shell {
		shellCmds := entrypoint.ShellPrint(o.Run)

		ientrypoint, cleanup, err := NewInteractiveEntrypoint(tmpDir, shellCmds, entrypoint)
		if err != nil {
			return err
		}
		defer cleanup()
		entrypointExec = ientrypoint
	}

	execArgs, err := entrypointExec.ExecArgs(EntrypointContext{
		Args:    o.Run,
		CmdArgs: o.TermArgs,
		Env:     o.Env,
	})
	if err != nil {
		return err
	}

	return executor.Exec(ctx, o, execArgs)
}
