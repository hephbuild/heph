package main

import (
	"github.com/hephbuild/heph/bootstrap"
	"github.com/hephbuild/heph/utils/xcontext"
	"github.com/hephbuild/heph/utils/xerrors"
	"github.com/hephbuild/heph/utils/xrand"
	"github.com/hephbuild/heph/vfssimple"
	"os"
)

func execute() error {
	ctx, cancel := xcontext.BootstrapSoftCancel()
	defer cancel(nil)

	vfssimple.WithContext(ctx)

	err := rootCmd.ExecuteContext(ctx)
	postRun(err)
	if err != nil {
		return err
	}

	return nil
}

func Execute() {
	xrand.Seed()

	if err := execute(); err != nil {
		exitCode := 1
		if eerr, ok := xerrors.As[bootstrap.ErrorWithExitCode](err); ok {
			exitCode = eerr.ExitCode
			// This is required in case ErrorWithExitCode does not have an Err set, just an ExitCode
			err = eerr.Err
		}
		bootstrap.PrintHumanError(err)

		os.Exit(exitCode)
	}
}
