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

func run(ctx context.Context, targetInvs []TargetInvocation, inlineSingle bool, shell bool) error {
	var inlineInvocationTarget *TargetInvocation
	var inlineTarget *engine.Target
	if len(targetInvs) == 1 && inlineSingle {
		inlineInvocationTarget = &targetInvs[0]
		inlineTarget = inlineInvocationTarget.Target
	}

	if shell && inlineInvocationTarget == nil {
		return fmt.Errorf("shell mode is only compatible with running a single target")
	}

	targets := make([]*engine.Target, 0)
	for _, inv := range targetInvs {
		targets = append(targets, inv.Target)
	}

	tdeps, err := Engine.ScheduleTargetsWithDeps(targets, inlineTarget)
	if err != nil {
		return err
	}

	deps := &worker.WaitGroup{}
	for _, target := range targets {
		deps.AddChild(tdeps[target])
	}

	err = WaitPool("Run", deps, false)
	if err != nil {
		printTargetErr(err)
		return err
	}

	if inlineInvocationTarget == nil {
		return nil
	}

	ideps := tdeps[inlineTarget]
	_ = ideps

	if !*porcelain {
		fmt.Println(inlineTarget.FQN)
	}

	e := engine.TargetRunEngine{
		Engine:  Engine,
		Context: ctx,
	}

	runner := e.Run
	if shell {
		runner = e.RunShell
	}

	err = runner(inlineTarget, sandbox.IOConfig{
		Stdin:  os.Stdin,
		Stdout: os.Stdout,
		Stderr: os.Stderr,
	}, inlineInvocationTarget.Args...)
	if err != nil {
		var eerr *exec.ExitError
		if errors.As(err, &eerr) {
			os.Exit(eerr.ExitCode())
		}

		return fmt.Errorf("%v: %w", inlineTarget.FQN, err)
	}

	return nil
}
