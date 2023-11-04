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

func generateRRs(ctx context.Context, g *graph.State, m specs.Matcher, args []string, opts targetrun.RequestOpts) (targetrun.Requests, error) {
	targets, err := g.Targets().Filter(m)
	if err != nil {
		return nil, err
	}

	rrs := make(targetrun.Requests, 0, targets.Len())
	for _, target := range targets.Slice() {
		if err := ctx.Err(); err != nil {
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

	return rrs, nil
}

func RunGen(ctx context.Context, e *scheduler.Scheduler, plain bool, filterFactory func() (func(gent *graph.Target) bool, error)) error {
	allGenTargets := sets.NewStringSet(0)

	for i := 0; ; i++ {
		filter, err := filterFactory()
		if err != nil {
			return err
		}

		genTargets := ads.Filter(e.Graph.GeneratedTargets(), func(gent *graph.Target) bool {
			if allGenTargets.Has(gent.Addr) {
				// Already ran gen
				return false
			}

			return filter(gent)
		})

		if len(genTargets) == 0 {
			break
		}

		for _, target := range genTargets {
			allGenTargets.Add(target.Addr)
			log.Warnf("GEN: %v", target.Addr)
		}

		// Run those gen targets
		deps, err := e.ScheduleGenPass(ctx, genTargets, false)
		if err != nil {
			return err
		}

		err = poolwait.Wait(ctx, fmt.Sprintf("Gen run %v", i), e.Pool, deps, plain)
		if err != nil {
			return err
		}
	}

	return nil
}

func GenerateRRs(ctx context.Context, e *scheduler.Scheduler, m specs.Matcher, targs []string, opts targetrun.RequestOpts, plain bool) (targetrun.Requests, error) {
	err := RunGen(ctx, e, plain, func() (func(gent *graph.Target) bool, error) {
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

		return func(gent *graph.Target) bool {
			gm := specs.OrNodeFactory(gent.Gen...)

			r := specs.Intersects(gm, requiredMatchers)

			if r.Bool() {
				return true
			}

			return false
		}, nil
	})
	if err != nil {
		return nil, err
	}

	return generateRRs(ctx, e.Graph, m, targs, opts)
}
