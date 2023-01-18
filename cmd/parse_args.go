package cmd

import (
	"bufio"
	"context"
	"fmt"
	"heph/engine"
	"heph/targetspec"
	"os"
	"strings"
)

// cache reading stdin
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
			if *ignoreUnknownTarget {
				continue
			}
			return nil, engine.NewTargetNotFoundError(tp.Full())
		}

		targets = append(targets, target)
	}

	return targets, nil
}

func parseTargetsAndArgs(ctx context.Context, args []string) (engine.TargetRunRequests, error) {
	err := engineInit(ctx)
	if err != nil {
		return nil, err
	}

	return parseTargetsAndArgsWithEngine(ctx, Engine, args)
}

func parseTargetsAndArgsWithEngine(ctx context.Context, e *engine.Engine, args []string) (engine.TargetRunRequests, error) {
	if hasStdin(args) {
		// Block and read stdin here to prevent multiple bubbletea running at the same time
		tps, err := parseTargetPathsFromStdin()
		if err != nil {
			return nil, err
		}

		if len(tps) == 0 {
			return nil, nil
		}
	}

	err := preRunWithGenWithOpts(ctx, PreRunOpts{
		Engine: e,
		Silent: false,
	})
	if err != nil {
		return nil, err
	}

	if hasStdin(args) {
		targets, err := parseTargetsFromStdin(e)
		if err != nil {
			return nil, err
		}

		rrs := make(engine.TargetRunRequests, 0)
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
		return nil, engine.NewTargetNotFoundError(tp.Full())
	}

	targs := args[1:]

	tnocache := *nocache
	if len(targs) > 0 {
		if !target.PassArgs {
			return nil, fmt.Errorf("%v does not allow args", target.FQN)
		}

		tnocache = true
	}

	return []engine.TargetRunRequest{{
		Target:  target,
		Args:    targs,
		NoCache: tnocache,
		Shell:   *shell,
	}}, nil
}
