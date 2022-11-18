package cmd

import (
	"context"
	"errors"
	"fmt"
	log "github.com/sirupsen/logrus"
	"heph/engine"
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
	if len(rrs) == 1 && inlineSingle {
		inlineInvocationTarget = &rrs[0]
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

	err = WaitPool("Run", e.Pool, deps, false)
	if err != nil {
		return err
	}

	if inlineInvocationTarget == nil {
		return nil
	}

	re := engine.TargetRunEngine{
		Engine: e,
		Print: func(s string) {
			log.Debug(s)
		},
	}

	err = re.Run(ctx, *inlineInvocationTarget, sandbox.IOConfig{
		Stdin:  os.Stdin,
		Stdout: os.Stdout,
		Stderr: os.Stderr,
	})
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

	return nil
}
