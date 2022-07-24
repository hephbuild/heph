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
	targets := make([]*engine.Target, 0)

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

		target := Engine.Targets.Find(tp.Full())
		if target == nil {
			return nil, engine.TargetNotFoundError(tp.Full())
		}

		targets = append(targets, target)
	}

	return targets, nil
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

func run(ctx context.Context, targets []TargetInvocation, fromStdin bool) error {
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()

	pool := worker.NewPool(ctx, *workers)
	defer pool.Stop()

	var inlineTarget *TargetInvocation
	if len(targets) == 1 && !fromStdin {
		inlineTarget = &targets[0]
	}

	for _, inv := range targets {
		_, err := Engine.ScheduleTargetDeps(ctx, pool, inv.Target)
		if err != nil {
			return err
		}

		if inlineTarget == nil || inv.Target != inlineTarget.Target {
			err := Engine.ScheduleTarget(ctx, pool, inv.Target)
			if err != nil {
				return err
			}
		}
	}

	if isTerm && !*plain {
		err := DynamicRenderer(ctx, cancel, pool)
		if err != nil {
			return fmt.Errorf("dynamic renderer: %w", err)
		}
	}
	<-pool.Done()

	if err := pool.Err; err != nil {
		if err, ok := err.(engine.TargetFailedError); ok {
			fmt.Printf("%v failed: %v\n", err.Target.FQN, err)
			logFile := err.Target.LogFile
			if logFile != "" {
				c := exec.Command("tail", "-n", "10", logFile)
				output, _ := c.Output()
				fmt.Println()
				fmt.Println(string(output))
				fmt.Printf("log file can be found at %v:\n", logFile)
			}
		}

		return err
	}

	if inlineTarget == nil {
		return nil
	}

	target := inlineTarget.Target

	fmt.Println(target.FQN)

	e := engine.TargetRunEngine{
		Engine:  Engine,
		Pool:    pool,
		Context: ctx,
	}

	err := e.Run(target, sandbox.IOConfig{
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

	<-pool.Done()

	return nil
}
