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

func generateRRs(ctx context.Context, g *graph.State, tps []specs.TargetPath, args []string, bailOutOnExpr bool, opts engine.TargetRunRequestOpts) (engine.TargetRunRequests, error) {
	targets := graph.NewTargets(len(tps))
	for _, tp := range tps {
		target := g.Targets().Find(tp.Full())
		if target == nil {
			return nil, engine.NewTargetNotFoundError(tp.Full(), g.Targets())
		}

		targets.Add(target)
	}

	check := func(target *graph.Target) error {
		if bailOutOnExpr {
			if len(target.Spec().Deps.Exprs) > 0 {
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
			log.Warnf("%v: args are being passed, disabling cache", target.FQN)
			rr.NoCache = true
		}

		rrs = append(rrs, rr)
	}

	ancs, err := g.DAG().GetOrderedAncestors(targets.Slice(), true)
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

func GenerateRRs(ctx context.Context, e *engine.Engine, tps []specs.TargetPath, targs []string, opts engine.TargetRunRequestOpts, plain bool) (engine.TargetRunRequests, error) {
	if len(tps) == 0 {
		return nil, nil
	}

	rrs, err := generateRRs(ctx, e.Graph, tps, targs, true, opts)
	if err == nil {
		return rrs, nil
	} else {
		log.Debugf("generateRRs: %v", err)
	}

	err = RunGen(ctx, e, "", false, plain)
	if err != nil {
		return nil, err
	}

	return generateRRs(ctx, e.Graph, tps, targs, false, opts)
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
