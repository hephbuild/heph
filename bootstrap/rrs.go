package bootstrap

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/engine"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/worker/poolwait"
)

func generateRRs(ctx context.Context, g *graph.State, m specs.Matcher, args []string, bailOutOnExpr bool, opts engine.TargetRunRequestOpts) (engine.TargetRunRequests, error) {
	targets, err := g.Targets().Filter(m)
	if err != nil {
		return nil, err
	}

	check := func(target *graph.Target) error {
		if bailOutOnExpr {
			if len(target.Spec().Deps.Exprs) > 0 {
				return fmt.Errorf("%v has expr, bailing out", target.Addr)
			}
		}

		return nil
	}

	rrs := make(engine.TargetRunRequests, 0, len(targets))
	for _, target := range targets {
		if err := ctx.Err(); err != nil {
			return nil, err
		}

		err := check(target)
		if err != nil {
			return nil, err
		}

		err = g.LinkTarget(target, nil)
		if err != nil {
			return nil, err
		}

		rr := engine.TargetRunRequest{
			Target:               target,
			Args:                 args,
			TargetRunRequestOpts: opts,
		}
		if len(rr.Args) > 0 && target.Cache.Enabled {
			log.Warnf("%v: args are being passed, disabling cache", target.Addr)
			rr.NoCache = true
		}

		rrs = append(rrs, rr)
	}

	ancs, err := g.DAG().GetOrderedAncestors(targets, true)
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

func GenerateRRs(ctx context.Context, e *engine.Engine, m specs.Matcher, targs []string, opts engine.TargetRunRequestOpts, plain bool) (engine.TargetRunRequests, error) {
	if specs.IsMatcherExplicit(m) {
		rrs, err := generateRRs(ctx, e.Graph, m, targs, true, opts)
		if err == nil {
			return rrs, nil
		} else {
			log.Debugf("generateRRs: %v", err)
		}
	}

	err := RunGen(ctx, e, "", false, plain)
	if err != nil {
		return nil, err
	}

	return generateRRs(ctx, e.Graph, m, targs, false, opts)
}

func RunGen(ctx context.Context, e *engine.Engine, poolName string, linkAll, plain bool) error {
	deps, err := e.ScheduleGenPass(ctx, linkAll)
	if err != nil {
		return err
	}

	if poolName == "" {
		poolName = "PreRun gen"
	}

	err = poolwait.Wait(ctx, poolName, e.Pool, deps, plain)
	if err != nil {
		return err
	}

	return nil
}
