package cmd

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	log "github.com/sirupsen/logrus"
	"heph/engine"
	"heph/sandbox"
	"heph/targetspec"
	"heph/worker"
	"os"
	"os/exec"
	"strings"
)

func hasStdin(args []string) bool {
	return len(args) == 1 && args[0] == "-"
}

func parseTargetsFromStdin(e *engine.Engine) ([]*engine.Target, error) {
	tps, err := parseTargetPathsFromStdin()
	if err != nil {
		return nil, err
	}

	targets := make([]*engine.Target, 0)

	for _, tp := range tps {
		target := e.Targets.Find(tp.Full())
		if target == nil {
			return nil, engine.TargetNotFoundError(tp.Full())
		}

		targets = append(targets, target)
	}

	return targets, nil
}

var targetsFromStdin []targetspec.TargetPath

func parseTargetPathsFromStdin() ([]targetspec.TargetPath, error) {
	if targetsFromStdin != nil {
		return targetsFromStdin, nil
	}

	s := bufio.NewScanner(os.Stdin)
	for s.Scan() {
		t := s.Text()
		t = strings.TrimSpace(t)

		if len(t) == 0 {
			continue
		}

		tp, err := targetspec.TargetParse("", t)
		if err != nil {
			return nil, err
		}

		targetsFromStdin = append(targetsFromStdin, tp)
	}

	return targetsFromStdin, nil
}

func parseTargetsAndArgs(e *engine.Engine, args []string) ([]engine.TargetRunRequest, error) {
	if hasStdin(args) {
		targets, err := parseTargetsFromStdin(e)
		if err != nil {
			return nil, err
		}

		rrs := make([]engine.TargetRunRequest, 0)
		for _, target := range targets {
			rrs = append(rrs, engine.TargetRunRequest{
				Target:  target,
				Args:    nil, // TODO
				NoCache: *nocache,
				Shell:   *shell,
			})
		}

		return rrs, nil
	}

	if len(args) == 0 {
		return nil, nil
	}

	tp, err := targetspec.TargetParse("", args[0])
	if err != nil {
		return nil, err
	}

	target := e.Targets.Find(tp.Full())
	if target == nil {
		return nil, engine.TargetNotFoundError(tp.Full())
	}

	targs := args[1:]

	if len(targs) > 0 && !target.PassArgs {
		return nil, fmt.Errorf("%v does not allow args", target.FQN)
	}

	return []engine.TargetRunRequest{{
		Target:  target,
		Args:    targs,
		NoCache: *nocache,
		Shell:   *shell,
	}}, nil
}

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
		Engine:  e,
		Context: ctx,
		Print: func(s string) {
			log.Debug(s)
		},
	}

	err = re.Run(*inlineInvocationTarget, sandbox.IOConfig{
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
