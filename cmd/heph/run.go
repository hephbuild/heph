package main

import (
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/engine"
	"github.com/hephbuild/heph/engine/graph"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/sandbox"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/worker"
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

func run(ctx context.Context, e *engine.Engine, rrs engine.TargetRunRequests, inlineSingle bool) error {
	return runMode(ctx, e, rrs, inlineSingle, "")
}

func runMode(ctx context.Context, e *engine.Engine, rrs engine.TargetRunRequests, inlineSingle bool, mode string) error {
	shellCount := rrs.Count(func(rr engine.TargetRunRequest) bool {
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

	var inlineRR *engine.TargetRunRequest
	var inlineTarget *graph.Target = nil
	if len(rrs) == 1 && inlineSingle && !*noInline {
		inlineRR = &rrs[0]
		inlineRR.Mode = mode
		inlineTarget = inlineRR.Target
	}

	// fgDeps will include deps created inside the scheduled jobs to be waited for in the foreground
	// The DoneSem() must be called after all the tdeps have finished
	ctx, fgDeps := engine.ContextWithForegroundWaitGroup(ctx)
	fgDeps.AddSem()

	// Passing inlineTarget directly results in a crash...
	var skip targetspec.Specer
	if inlineTarget != nil {
		skip = inlineTarget
	}
	tdepsMap, err := e.ScheduleTargetRRsWithDeps(ctx, rrs, skip)
	if err != nil {
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

	err = WaitPool("Run", e.Pool, runDeps)
	if err != nil {
		return err
	}

	if inlineRR == nil {
		if printOutput.bool {
			for _, target := range rrs.Targets().Slice() {
				target := e.Targets.FindGraph(target)
				err = printTargetOutputPaths(target, printOutput.str)
				if err != nil {
					return err
				}
			}
		}

		if catOutput.bool {
			for _, target := range rrs.Targets().Slice() {
				target := e.Targets.FindGraph(target)
				err = printTargetOutputContent(target, catOutput.str)
				if err != nil {
					return err
				}
			}
		}

		return nil
	}

	cfg := sandbox.IOConfig{
		Stdout: os.Stdout,
		Stderr: os.Stderr,
		Stdin:  os.Stdin,
	}
	if printOutput.bool || catOutput.bool {
		log.Debugf("Redirecting stdout to stderr")
		cfg.Stdout = os.Stderr
	}

	err = e.Run(ctx, *inlineRR, cfg)
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

	inlineTargetEngine := e.Targets.FindGraph(inlineTarget)

	if printOutput.bool {
		err = printTargetOutputPaths(inlineTargetEngine, printOutput.str)
		if err != nil {
			return err
		}
	} else if catOutput.bool {
		err = printTargetOutputContent(inlineTargetEngine, catOutput.str)
		if err != nil {
			return err
		}
	}

	return nil
}
