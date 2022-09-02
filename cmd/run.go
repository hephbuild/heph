package cmd

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"heph/engine"
	"heph/sandbox"
	"heph/utils"
	"heph/worker"
	"os"
	"os/exec"
	"strings"
)

type TargetInvocation struct {
	Target *engine.Target
	Args   []string
}

func hasStdin(args []string) bool {
	return len(args) == 1 && args[0] == "-"
}

func parseTargetsFromStdin() ([]*engine.Target, error) {
	tps, err := parseTargetPathsFromStdin()
	if err != nil {
		return nil, err
	}

	targets := make([]*engine.Target, 0)

	for _, tp := range tps {
		target := Engine.Targets.Find(tp.Full())
		if target == nil {
			return nil, engine.TargetNotFoundError(tp.Full())
		}

		targets = append(targets, target)
	}

	return targets, nil
}

var targetsFromStdin []utils.TargetPath

func parseTargetPathsFromStdin() ([]utils.TargetPath, error) {
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

		tp, err := utils.TargetParse("", t)
		if err != nil {
			return nil, err
		}

		targetsFromStdin = append(targetsFromStdin, tp)
	}

	return targetsFromStdin, nil
}

func parseTargetsAndArgs(args []string) ([]TargetInvocation, error) {
	if hasStdin(args) {
		targets, err := parseTargetsFromStdin()
		if err != nil {
			return nil, err
		}

		targetInvs := make([]TargetInvocation, 0)
		for _, target := range targets {
			targetInvs = append(targetInvs, TargetInvocation{
				Target: target,
				Args:   nil, // TODO
			})
		}

		return targetInvs, nil
	}

	if len(args) == 0 {
		return nil, nil
	}

	tp, err := utils.TargetParse("", args[0])
	if err != nil {
		return nil, err
	}

	target := Engine.Targets.Find(tp.Full())
	if target == nil {
		return nil, engine.TargetNotFoundError(tp.Full())
	}

	targs := args[1:]

	if len(targs) > 0 && !target.PassArgs {
		return nil, fmt.Errorf("%v does not allow args", target.FQN)
	}

	return []TargetInvocation{{
		Target: target,
		Args:   targs,
	}}, nil
}

func run(ctx context.Context, targetInvs []TargetInvocation, fromStdin bool) error {
	var inlineTarget *TargetInvocation
	if len(targetInvs) == 1 && !fromStdin {
		inlineTarget = &targetInvs[0]
	}

	targets := make(engine.Targets, 0)
	for _, inv := range targetInvs {
		targets = append(targets, inv.Target)
	}

	deps := &worker.WaitGroup{}

	tdeps, err := Engine.ScheduleTargetsDeps(targets)
	if err != nil {
		return err
	}
	deps.AddChild(tdeps)

	for _, inv := range targetInvs {
		if inlineTarget == nil || inv.Target != inlineTarget.Target {
			j, err := Engine.ScheduleTarget(inv.Target)
			if err != nil {
				return err
			}
			deps.Add(j)
		}
	}

	err = WaitPool("Run", deps, false)
	if err != nil {
		printTargetErr(err)
		return err
	}

	if inlineTarget == nil {
		return nil
	}

	target := inlineTarget.Target

	if !*porcelain {
		fmt.Println(target.FQN)
	}

	e := engine.TargetRunEngine{
		Engine:  Engine,
		Context: ctx,
	}

	err = e.Run(target, sandbox.IOConfig{
		Stdin:  os.Stdin,
		Stdout: os.Stdout,
		Stderr: os.Stderr,
	}, inlineTarget.Args...)
	if err != nil {
		var eerr *exec.ExitError
		if errors.As(err, &eerr) {
			os.Exit(eerr.ExitCode())
		}

		return fmt.Errorf("%v: %w", target.FQN, err)
	}

	return nil
}
