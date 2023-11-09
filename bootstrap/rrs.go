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

func generateRRs(ctx context.Context, g *graph.State, m specs.Matcher, args []string, opts targetrun.RequestOpts, bailOutOnExpr bool) (targetrun.Requests, error) {
	targets, err := g.Targets().Filter(m)
	if err != nil {
		return nil, err
	}

	check := func(target *graph.Target) error { return nil }
	if bailOutOnExpr {
		check = func(target *graph.Target) error {
			if len(target.Spec().Deps.Exprs) > 0 {
				return fmt.Errorf("%v: %w", target.Addr, errHasExprDep)
			}

			return nil
		}
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

	if bailOutOnExpr {
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
	}

	return rrs, nil
}

func RunAllGen(ctx context.Context, e *scheduler.Scheduler, plain bool) error {
	err := RunGen(ctx, e, plain, func() (func(gent *graph.Target) bool, error) {
		return func(gent *graph.Target) bool {
			return true
		}, nil
	})
	if err != nil {
		return err
	}

	return nil
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
			log.Debugf("RG: GEN: %v", target.Addr)
		}

		// Run those gen targets
		deps, err := e.ScheduleGenPass(ctx, genTargets)
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

func kindMatcher(gent *graph.Target) specs.KindMatcher {
	gm := specs.KindMatcher{}
	for _, m := range gent.Gen {
		gm.Add(m)
	}
	return gm
}

func Query(ctx context.Context, e *scheduler.Scheduler, m specs.Matcher, plain bool) ([]specs.Target, error) {
	mSimpl := m.Simplify()

	err := RunGen(ctx, e, plain, func() (func(gent *graph.Target) bool, error) {
		return func(gent *graph.Target) bool {
			gm := kindMatcher(gent)

			r := specs.Intersects(gm, mSimpl)

			return r.Bool()
		}, nil
	})
	if err != nil {
		return nil, err
	}

	targets, err := e.Graph.Targets().Filter(m)
	if err != nil {
		return nil, err
	}

	targetSpecs := ads.Map(targets.Slice(), func(t *graph.Target) specs.Target {
		return t.Target
	})

	return targetSpecs, nil
}

func GenerateRRs(ctx context.Context, e *scheduler.Scheduler, m specs.Matcher, targs []string, opts targetrun.RequestOpts, plain bool) (targetrun.Requests, error) {
	if !e.Config.Engine.SmartGen {
		if specs.IsMatcherExplicit(m) {
			rrs, err := generateRRs(ctx, e.Graph, m, targs, opts, true)
			if err != nil {
				if !(errors.Is(err, errHasExprDep) || errors.Is(err, specs.TargetNotFoundErr{})) {
					return nil, err
				}
				log.Debugf("generateRRs: %v", err)
			} else {
				return rrs, nil
			}

			for _, target := range e.Graph.Targets().Slice() {
				target.ResetLinking()
			}
		}
	}

	err := RunGen(ctx, e, plain, func() (func(gent *graph.Target) bool, error) {
		if !e.Config.Engine.SmartGen {
			return func(gent *graph.Target) bool {
				return true
			}, nil
		}

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

		requiredMatchersSimpl := requiredMatchers.Simplify()

		log.Debug("GRR:  M:", requiredMatchers.String())
		if requiredMatchers.String() != requiredMatchersSimpl.String() {
			log.Debug("GRR: MS:", requiredMatchersSimpl.String())
		}

		return func(gent *graph.Target) bool {
			gm := kindMatcher(gent)

			r := specs.Intersects(gm, requiredMatchersSimpl)

			return r.Bool()
		}, nil
	})
	if err != nil {
		return nil, err
	}

	return generateRRs(ctx, e.Graph, m, targs, opts, false)
}
