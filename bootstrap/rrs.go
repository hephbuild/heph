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
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/sets"
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
	allGenTargets := sets.NewStringSet(0)

	for i := 0; ; i++ {
		targets, err := e.Graph.Targets().Filter(m)
		if err != nil {
			if !errors.Is(err, specs.TargetNotFoundErr{}) {
				return nil, err
			}
		}

		var requiredMatchers = m
		if targets != nil {
			ms, err := e.Graph.RequiredMatchers(targets.Slice())
			if err != nil {
				return nil, err
			}

			requiredMatchers = specs.OrNodeFactory(requiredMatchers, ms)
		}

		genTargets := ads.Filter(e.Graph.GeneratedTargets(), func(gent *graph.Target) bool {
			if allGenTargets.Has(gent.Addr) {
				// Already ran gen
				return false
			}

			gm := specs.OrNodeFactory(gent.Gen...)

			return gm.Intersects(requiredMatchers)
		})

		if len(genTargets) == 0 {
			break
		}

		for _, target := range genTargets {
			allGenTargets.Add(target.Addr)
		}

		// Run those gen targets
		deps, err := e.ScheduleGenPass(ctx, genTargets, false)
		if err != nil {
			return nil, err
		}

		err = poolwait.Wait(ctx, fmt.Sprintf("PreRun gen %v", i), e.Pool, deps, plain)
		if err != nil {
			return nil, err
		}
	}

	return generateRRs(ctx, e.Graph, m, targs, false, opts)
}
