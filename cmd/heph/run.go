package main

import (
	"context"
	"errors"
	"fmt"
	"heph/engine"
	log "heph/hlog"
	"heph/sandbox"
	"heph/worker"
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

	var inlineInvocationTarget *engine.TargetRunRequest
	var inlineTarget *engine.Target
	if len(rrs) == 1 && inlineSingle && !*noInline {
		inlineInvocationTarget = &rrs[0]
		inlineInvocationTarget.Mode = mode
		inlineTarget = inlineInvocationTarget.Target
	}

	targets := rrs.Targets()

	tdeps, err := e.ScheduleTargetRRsWithDeps(ctx, rrs, inlineTarget)
	if err != nil {
		return err
	}

	deps := &worker.WaitGroup{}
	for _, target := range targets {
		deps.AddChild(tdeps.Get(target.FQN))
	}

	err = WaitPool("Run", e.Pool, deps)
	if err != nil {
		return err
	}

	if inlineInvocationTarget == nil {
		if printOutput.bool {
			for _, target := range rrs.Targets() {
				err = printTargetOutput(target, printOutput.str)
				if err != nil {
					return err
				}
			}
		}

		return nil
	}

	re := engine.NewTargetRunEngine(e, func(s worker.Status) {
		log.Info(s.String(isTerm))
	})

	cfg := sandbox.IOConfig{
		Stdout: os.Stdout,
		Stderr: os.Stderr,
		Stdin:  os.Stdin,
	}
	if printOutput.bool {
		log.Debugf("Redirecting stdout to stderr")
		cfg.Stdout = os.Stderr
	}

	err = re.Run(ctx, *inlineInvocationTarget, cfg)
	if err != nil {
		var eerr *exec.ExitError
		if errors.As(err, &eerr) {
			return ErrorWithExitCode{
				Err:      err,
				ExitCode: eerr.ExitCode(),
			}
		}

		return fmt.Errorf("%v: %w", inlineTarget.FQN, err)
	}

	if printOutput.bool {
		err = printTargetOutput(inlineTarget, printOutput.str)
		if err != nil {
			return err
		}
	}

	return nil
}
