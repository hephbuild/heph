package engine

import (
	"context"
	"fmt"
	"heph/utils/maps"
	"heph/utils/sets"
	"heph/worker"
)

type schedEngine struct {
	*Engine

	rrs        TargetRunRequests
	targets    []*Target
	allTargets []*Target
	skip       *Target

	targetDeps         *WaitGroupMap
	targetAnalysisDeps *WaitGroupMap
	pullMetaDeps       *WaitGroupMap
	allPullMetaDeps    *worker.WaitGroup

	allScheduled *worker.WaitGroup

	requiredOutputs *maps.Map[string, *sets.Set[string]]

	needRun       *Targets
	needCacheWarm *sets.Set[TargetWithOutput]
}

func (e *Engine) ScheduleV2TargetRRsWithDeps(octx context.Context, rrs TargetRunRequests, skip *Target) (*WaitGroupMap, error) {
	se, err := newSchedEngine(e, rrs, skip)
	if err != nil {
		return nil, err
	}

	return se.scheduleTargetRRsWithDeps(octx)
}

func newSchedEngine(e *Engine, rrs TargetRunRequests, skip *Target) (*schedEngine, error) {
	targets := rrs.Targets()

	allTargets, requiredOutputs, err := e.DAG().GetOrderedAncestorsWithOutput(targets, true)
	if err != nil {
		return nil, err
	}

	return &schedEngine{
		Engine:             e,
		rrs:                rrs,
		targets:            targets,
		allTargets:         allTargets,
		skip:               skip,
		targetDeps:         &WaitGroupMap{},
		targetAnalysisDeps: &WaitGroupMap{},
		pullMetaDeps:       &WaitGroupMap{},
		allPullMetaDeps:    &worker.WaitGroup{},
		allScheduled:       &worker.WaitGroup{},
		requiredOutputs:    requiredOutputs,
		needRun:            NewTargets(0),
		needCacheWarm: sets.NewSet[TargetWithOutput](func(t TargetWithOutput) string {
			return t.Full()
		}, 0),
	}, nil
}

func (e *schedEngine) isSkip(target *Target) bool {
	return e.skip != nil && e.skip.FQN == target.FQN
}

func (e *schedEngine) scheduleRun(ctx context.Context, target *Target) error {
	if !e.needRun.Add(target) {
		return nil
	}

	parents, err := e.DAG().GetParents(target)
	if err != nil {
		return err
	}

	rdeps := &worker.WaitGroup{}
	for _, parent := range parents {
		rdeps.AddChild(e.targetDeps.Get(parent.FQN))
	}

	j, err := e.ScheduleTarget(ctx, e.rrs.Get(target), rdeps)
	if err != nil {
		return err
	}

	e.targetDeps.Get(target.FQN).Add(j)
	e.targetAnalysisDeps.Get(target.FQN).DoneSem()

	return nil
}

func (e *schedEngine) scheduleCacheWarm(ctx context.Context, target *Target, outputs []string) error {
	deps := &worker.WaitGroup{}
	for _, output := range outputs {
		output := output

		added := e.needCacheWarm.Add(TargetWithOutput{
			Target: target,
			Output: output,
		})
		if !added {
			continue
		}

		parents, err := e.DAG().GetParents(target)
		if err != nil {
			return err
		}

		wdeps := &worker.WaitGroup{}
		for _, parent := range parents {
			wdeps.AddChild(e.targetDeps.Get(parent.FQN))
		}

		j := e.Pool.Schedule(ctx, &worker.Job{
			Name: "warm " + target.FQN + "|" + output,
			Deps: wdeps,
			Do: func(w *worker.Worker, ctx context.Context) error {
				re := TargetRunEngine{
					Engine: e.Engine,
					Print:  w.Status,
				}

				w.Status(fmt.Sprintf("Priming cache %v|%v...", target.FQN, output))

				cached, err := re.WarmTargetCache(ctx, target, output)
				if err != nil {
					return err
				}

				if cached {
					// WarmCache
					return nil
				}

				return nil
			},
		})
		deps.Add(j)
	}

	return nil
}

func (e *schedEngine) scheduleAnalysis(ctx context.Context, target *Target) error {
	children, err := e.DAG().GetChildren(target)
	if err != nil {
		return err
	}

	analysisDep := &worker.WaitGroup{}
	for _, child := range children {
		wg := e.targetAnalysisDeps.Get(child.FQN)
		analysisDep.AddChild(wg)
	}

	var deps *worker.WaitGroup
	if target.Cache.Enabled {
		// TODO
		//outputDeps := &worker.WaitGroup{}
		//for _, output := range target.OutWithSupport.Names() {
		//	deps.AddChild()
		//}

		deps = worker.WaitGroupOr(analysisDep)
	} else {
		deps = analysisDep
	}

	j := e.Pool.Schedule(ctx, &worker.Job{
		Name: "analysis " + target.FQN,
		Deps: deps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			if e.needRun.Has(target) {
				return nil
			}

			e.targetAnalysisDeps.Get(target.FQN).DoneSem()

			return nil
		},
	})

	e.targetDeps.Get(target.FQN).Add(j)
	return nil
}

func (e *schedEngine) schedulePullMeta(ctx context.Context, target *Target) error {
	parents, err := e.DAG().GetParents(target)
	if err != nil {
		return err
	}

	pmdeps := &worker.WaitGroup{}
	for _, parent := range parents {
		pmdeps.AddChild(e.pullMetaDeps.Get(parent.FQN))
	}

	j := e.Pool.Schedule(ctx, &worker.Job{
		Name: "pull_meta " + target.FQN,
		Deps: pmdeps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			hasParentCacheMiss := false
			for _, parent := range parents {
				if e.needRun.Find(parent.FQN) != nil {
					hasParentCacheMiss = true
					break
				}
			}

			rr := e.rrs.Get(target)
			if !e.isSkip(target) && !hasParentCacheMiss && target.Cache.Enabled && !rr.NoCache {
				w.Status(fmt.Sprintf("Fetching meta %v...", target.FQN))

				re := TargetRunEngine{
					Engine: e.Engine,
					Print:  w.Status,
				}

				outputs := e.requiredOutputs.Get(target.FQN).Slice()
				cached, err := re.PullTargetMeta(ctx, target, outputs)
				if err != nil {
					return fmt.Errorf("pullmeta: %w", err)
				}

				if cached {
					if Contains(e.targets, target.FQN) {
						err := e.scheduleCacheWarm(ctx, target, target.OutWithSupport.Names())
						if err != nil {
							return err
						}
					}
					return nil
				}
			}

			for _, parent := range parents {
				outputs := e.requiredOutputs.Get(parent.FQN).Slice()

				err = e.scheduleCacheWarm(ctx, parent, outputs)
				if err != nil {
					return err
				}
			}

			err = e.scheduleRun(ctx, target)
			if err != nil {
				return err
			}

			return nil
		},
	})
	e.pullMetaDeps.Get(target.FQN).Add(j)

	return nil
}

func (e *schedEngine) scheduleTargetRRsWithDeps(octx context.Context) (_ *WaitGroupMap, rerr error) {
	ctx, span := e.SpanScheduleTargetWithDeps(octx, e.targets)
	defer func() {
		if rerr != nil {
			span.EndError(rerr)
		}
	}()

	e.allScheduled.AddSem()
	defer e.allScheduled.DoneSem()

	for _, target := range e.allTargets {
		// Will need to be cleared by a schedulerun or a successful cache pull of all the outputs
		e.targetAnalysisDeps.Get(target.FQN).AddSem()
	}

	for _, target := range e.allTargets {
		err := e.schedulePullMeta(ctx, target)
		if err != nil {
			return nil, err
		}

		err = e.scheduleAnalysis(ctx, target)
		if err != nil {
			return nil, err
		}
	}

	return e.targetDeps, nil
}
