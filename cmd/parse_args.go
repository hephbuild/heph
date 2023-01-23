package cmd

import (
	"bufio"
	"context"
	"fmt"
	"heph/engine"
	log "heph/hlog"
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

func generateRRs(ctx context.Context, e *engine.Engine, tps []targetspec.TargetPath, args []string, bailOutOnExpr bool) (engine.TargetRunRequests, error) {
	targets := engine.NewTargets(len(tps))
	for _, tp := range tps {
		target := e.Targets.Find(tp.Full())
		if target == nil {
			return nil, engine.NewTargetNotFoundError(tp.Full())
		}

		targets.Add(target)
	}

	rrs := make(engine.TargetRunRequests, 0)
	for _, target := range targets.Slice() {
		if bailOutOnExpr {
			if len(target.TargetSpec.Deps.Exprs) > 0 {
				return nil, fmt.Errorf("%v has expr, bailing out", target.FQN)
			}
		}

		err := e.LinkTarget(target, nil)
		if err != nil {
			return nil, err
		}

		rrs = append(rrs, engine.TargetRunRequest{
			Target:  target,
			Args:    args,
			NoCache: *nocache,
			Shell:   *shell,
		})
	}

	err := e.CreateDag()
	if err != nil {
		return nil, err
	}

	return rrs, nil
}

func parseTargetsAndArgsWithEngine(ctx context.Context, e *engine.Engine, args []string) (engine.TargetRunRequests, error) {
	var tps []targetspec.TargetPath
	var targs []string
	if hasStdin(args) {
		// Block and read stdin here to prevent multiple bubbletea running at the same time
		var err error
		tps, err = parseTargetPathsFromStdin()
		if err != nil {
			return nil, err
		}
	} else {
		if len(args) == 0 {
			return nil, nil
		}

		tp, err := targetspec.TargetParse("", args[0])
		if err != nil {
			return nil, err
		}
		tps = []targetspec.TargetPath{tp}

		targs = args[1:]
	}

	if len(tps) == 0 {
		return nil, nil
	}

	err := engineInitWithEngine(ctx, e)
	if err != nil {
		return nil, err
	}

	rrs, err := generateRRs(ctx, e, tps, targs, true)
	if err == nil {
		return rrs, nil
	} else {
		log.Debugf("generateRRs: %v", err)
	}

	err = preRunWithGenWithOpts(ctx, PreRunOpts{
		Engine: e,
		Silent: false,
	})
	if err != nil {
		return nil, err
	}

	return generateRRs(ctx, e, tps, targs, false)
}
