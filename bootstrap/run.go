package bootstrap

import (
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/sandbox"
	"github.com/hephbuild/heph/scheduler"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/targetrun"
	"github.com/hephbuild/heph/worker"
	"github.com/hephbuild/heph/worker/poolwait"
	"os"
	"os/exec"
)

type ErrorWithExitCode struct {
	Err      error
	ExitCode int
}

func (e ErrorWithExitCode) Error() string {
	return e.Err.Error()
}

func (e ErrorWithExitCode) Unwrap() error {
	return e.Err
}

func Run(ctx context.Context, e *scheduler.Scheduler, rrs targetrun.Requests, runopts RunOpts, inlineSingle bool) error {
	return RunMode(ctx, e, rrs, runopts, inlineSingle, "", sandbox.IOConfig{
		Stdout: os.Stdout,
		Stderr: os.Stderr,
		Stdin:  os.Stdin,
	})
}

func RunMode(ctx context.Context, e *scheduler.Scheduler, rrs targetrun.Requests, runopts RunOpts, inlineSingle bool, mode string, iocfg sandbox.IOConfig) error {
	for i := range rrs {
		rrs[i].Mode = mode
	}

	shellCount := rrs.Count(func(rr targetrun.Request) bool {
		return rr.Shell
	})

	if shellCount > 0 {
		if shellCount > 1 {
			return fmt.Errorf("shell mode is only compatible with running a single target")
		}

		if !inlineSingle {
			return fmt.Errorf("target invocation must be inlined to enable shell")
		}
	}

	var inlineRR *targetrun.Request
	if len(rrs) == 1 && inlineSingle && !runopts.NoInline {
		inlineRR = &rrs[0]
	}

	// fgDeps will include deps created inside the scheduled jobs to be waited for in the foreground
	// The DoneSem() must be called after all the tdeps have finished
	ctx, fgDeps := poolwait.ContextWithForegroundWaitGroup(ctx)
	fgDeps.AddSem()

	var skip []specs.Specer
	if inlineRR != nil {
		skip = []specs.Specer{inlineRR.Target}
	}
	tdepsMap, err := e.ScheduleTargetRRsWithDeps(ctx, rrs, skip)
	if err != nil {
		fgDeps.DoneSem()
		return err
	}

	tdeps := tdepsMap.All()
	go func() {
		<-tdeps.Done()
		fgDeps.DoneSem()
	}()

	runDeps := &worker.WaitGroup{}
	runDeps.AddChild(tdeps)
	runDeps.AddChild(fgDeps)

	err = poolwait.Wait(ctx, "Run", e.Pool, runDeps, runopts.Plain)
	if err != nil {
		return err
	}

	if inlineRR == nil {
		if runopts.PrintOutput.Bool {
			for _, target := range rrs.Targets().Slice() {
				target := e.LocalCache.Metas.Find(target)
				err = PrintTargetOutputPaths(target, runopts.PrintOutput.Str)
				if err != nil {
					return err
				}
			}
		}

		if runopts.CatOutput.Bool {
			for _, target := range rrs.Targets().Slice() {
				target := e.LocalCache.Metas.Find(target)
				err = PrintTargetOutputContent(target, runopts.CatOutput.Str)
				if err != nil {
					return err
				}
			}
		}

		return nil
	}

	if runopts.PrintOutput.Bool || runopts.CatOutput.Bool {
		log.Debugf("Redirecting stdout to stderr")
		iocfg.Stdout = os.Stderr
	}

	err = e.RunWithSpan(ctx, *inlineRR, iocfg)
	if err != nil {
		var eerr *exec.ExitError
		if errors.As(err, &eerr) {
			return ErrorWithExitCode{
				Err:      err,
				ExitCode: eerr.ExitCode(),
			}
		}

		return err
	}

	inlineTarget := e.LocalCache.Metas.Find(inlineRR.Target)

	if runopts.PrintOutput.Bool {
		err = PrintTargetOutputPaths(inlineTarget, runopts.PrintOutput.Str)
		if err != nil {
			return err
		}
	} else if runopts.CatOutput.Bool {
		err = PrintTargetOutputContent(inlineTarget, runopts.CatOutput.Str)
		if err != nil {
			return err
		}
	}

	return nil
}
