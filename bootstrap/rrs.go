package bootstrap

import (
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/scheduler"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/targetrun"
	"github.com/hephbuild/heph/worker/poolwait"
)

var errHasExprDep = errors.New("has expr, bailing out")

func generateRRs(ctx context.Context, g *graph.State, m specs.Matcher, args []string, bailOutOnExpr bool, opts targetrun.RequestOpts) (targetrun.Requests, error) {
	targets, err := g.Targets().Filter(m)
	if err != nil {
		return nil, err
	}

	check := func(target *graph.Target) error {
		if bailOutOnExpr {
			if len(target.Spec().Deps.Exprs) > 0 {
				return fmt.Errorf("%v: %w", target.Addr, errHasExprDep)
			}
		}

		return nil
	}

	rrs := make(targetrun.Requests, 0, targets.Len())
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

		rr := targetrun.Request{
			Target:      target,
			Args:        args,
			RequestOpts: opts,
		}
		if len(rr.Args) > 0 && target.Cache.Enabled {
			log.Warnf("%v: args are being passed, disabling cache", target.Addr)
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

func GenerateRRs(ctx context.Context, e *scheduler.Scheduler, m specs.Matcher, targs []string, opts targetrun.RequestOpts, plain bool) (targetrun.Requests, error) {
	if specs.IsMatcherExplicit(m) {
		rrs, err := generateRRs(ctx, e.Graph, m, targs, true, opts)
		if err != nil {
			if !(errors.Is(err, errHasExprDep) || errors.Is(err, specs.TargetNotFoundErr{})) {
				return nil, err
			}
			log.Debugf("generateRRs: %v", err)
		} else {
			return rrs, nil
		}
	}

	err := runGen(ctx, e, "", false, plain)
	if err != nil {
		return nil, err
	}

	return generateRRs(ctx, e.Graph, m, targs, false, opts)
}

func runGen(ctx context.Context, e *scheduler.Scheduler, poolName string, linkAll, plain bool) error {
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
