package main

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/bootstrap"
	"github.com/hephbuild/heph/engine"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/log/log"
)

// Deprecated: use bootstrap.BlockReadStdin
func blockReadStdin(args []string) error {
	return bootstrap.BlockReadStdin(args)
}

// Deprecated: use bootstrap.HasStdin
func hasStdin(args []string) bool {
	return bootstrap.HasStdin(args)
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

func parseTargetsAndArgsWithEngine(ctx context.Context, e *engine.Engine, args []string, stdin bool) (engine.TargetRunRequests, error) {
	tps, targs, err := bootstrap.ParseTargetPathsAndArgs(args, stdin)
	if err != nil {
		return nil, err
	}

	if len(tps) == 0 {
		return nil, nil
	}

	return bootstrap.GenerateRRs(ctx, e, tps, targs, engine.TargetRunRequestOpts{
		NoCache:       *nocache,
		Shell:         *shell,
		PreserveCache: printOutput.bool || catOutput.bool,
		NoPTY:         *nopty,
	}, *plain)
}
