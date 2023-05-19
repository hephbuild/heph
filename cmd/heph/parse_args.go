package main

import (
	"bufio"
	"context"
	"fmt"
	"github.com/hephbuild/heph/bootstrap"
	"github.com/hephbuild/heph/engine"
	"github.com/hephbuild/heph/engine/graph"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/tgt"
	"os"
	"strings"
)

// cache reading stdin
var targetsFromStdin []targetspec.TargetPath

func blockReadStdin(args []string) error {
	if hasStdin(args) {
		_, err := parseTargetPathsFromStdin()
		if err != nil {
			return err
		}
	}

	return nil
}

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

func parseTargetsFromStdin(gtargets *graph.Targets) ([]*tgt.Target, error) {
	tps, err := parseTargetPathsFromStdin()
	if err != nil {
		return nil, err
	}

	targets := make([]*tgt.Target, 0)

	for _, tp := range tps {
		target := gtargets.Find(tp.Full())
		if target == nil {
			if *ignoreUnknownTarget {
				continue
			}
			return nil, engine.NewTargetNotFoundError(tp.Full(), gtargets)
		}

		targets = append(targets, target.Target)
	}

	return targets, nil
}

func parseTargetFromArgs(ctx context.Context, args []string) (bootstrap.EngineBootstrap, *graph.Target, error) {
	if len(args) != 1 {
		return bootstrap.EngineBootstrap{}, nil, fmt.Errorf("expected one arg")
	}

	err := blockReadStdin(args)
	if err != nil {
		return bootstrap.EngineBootstrap{}, nil, err
	}

	bs, err := engineInit(ctx)
	if err != nil {
		return bs, nil, err
	}

	rrs, err := parseTargetsAndArgsWithEngine(ctx, bs.Engine, args, false)
	if err != nil {
		return bs, nil, err
	}

	if len(rrs) != 1 {
		log.Debugf("no rrs: %v", args)

		s := ""
		if len(args) > 0 {
			s = args[0]
		}
		return bs, nil, engine.NewTargetNotFoundError(s, bs.Graph.Targets())
	}

	return bs, rrs[0].Target, nil
}

func parseTargetsAndArgs(ctx context.Context, args []string) (bootstrap.EngineBootstrap, engine.TargetRunRequests, error) {
	err := blockReadStdin(args)
	if err != nil {
		return bootstrap.EngineBootstrap{}, nil, err
	}

	bs, err := engineInit(ctx)
	if err != nil {
		return bootstrap.EngineBootstrap{}, nil, err
	}

	rrs, err := parseTargetsAndArgsWithEngine(ctx, bs.Engine, args, true)
	if err != nil {
		return bootstrap.EngineBootstrap{}, nil, err
	}

	return bs, rrs, err
}

func generateRRs(ctx context.Context, e *engine.Engine, tps []targetspec.TargetPath, args []string, bailOutOnExpr bool) (engine.TargetRunRequests, error) {
	targets := graph.NewTargets(len(tps))
	for _, tp := range tps {
		target := e.Graph.Targets().Find(tp.Full())
		if target == nil {
			return nil, engine.NewTargetNotFoundError(tp.Full(), e.Graph.Targets())
		}

		targets.Add(target)
	}

	check := func(target *graph.Target) error {
		if bailOutOnExpr {
			if len(target.TargetSpec.Deps.Exprs) > 0 {
				return fmt.Errorf("%v has expr, bailing out", target.FQN)
			}
		}

		return nil
	}

	rrs := make(engine.TargetRunRequests, 0, targets.Len())
	for _, target := range targets.Slice() {
		if err := ctx.Err(); err != nil {
			return nil, err
		}

		err := check(target)
		if err != nil {
			return nil, err
		}

		err = e.Graph.LinkTarget(target, nil)
		if err != nil {
			return nil, err
		}

		rr := engine.TargetRunRequest{
			Target:        target,
			Args:          args,
			NoCache:       *nocache,
			Shell:         *shell,
			PreserveCache: printOutput.bool || catOutput.bool,
			NoPTY:         *nopty,
		}
		if len(rr.Args) > 0 && target.Cache.Enabled {
			log.Warnf("%v: args are being passed, disabling cache", target.FQN)
			rr.NoCache = true
		}

		rrs = append(rrs, rr)
	}

	ancs, err := e.Graph.DAG().GetOrderedAncestors(targets.Slice(), true)
	if err != nil {
		return nil, err
	}

	for _, anc := range ancs {
		err := check(anc)
		if err != nil {
			return nil, err
		}
	}

	return rrs, nil
}

func parseTargetsAndArgsWithEngine(ctx context.Context, e *engine.Engine, args []string, stdin bool) (engine.TargetRunRequests, error) {
	var tps []targetspec.TargetPath
	var targs []string
	if stdin && hasStdin(args) {
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

	rrs, err := generateRRs(ctx, e, tps, targs, true)
	if err == nil {
		return rrs, nil
	} else {
		log.Debugf("generateRRs: %v", err)
	}

	err = preRunWithGenWithOpts(ctx, PreRunOpts{
		Engine: e,
	})
	if err != nil {
		return nil, err
	}

	return generateRRs(ctx, e, tps, targs, false)
}
