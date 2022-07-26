package engine

import (
	"context"
	"fmt"
	"heph/utils"
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

	allScheduled *worker.WaitGroup

	requiredOutputs *maps.Map[string, *sets.Set[string, string]]

	needRun       *Targets
	needCacheWarm *sets.Set[string, TargetWithOutput]
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
		allScheduled:       &worker.WaitGroup{},
		requiredOutputs:    requiredOutputs,
		needRun:            NewTargets(0),
		needCacheWarm: sets.NewSet[string, TargetWithOutput](func(t TargetWithOutput) string {
			return t.Full()
		}, 0),
	}, nil
}

func (e *schedEngine) isSkip(target *Target) bool {
	return e.skip != nil && e.skip.FQN == target.FQN
}

func (e *schedEngine) scheduleRun(ctx context.Context, target *Target) error {
	parents, err := e.DAG().GetParents(target)
	if err != nil {
		return err
	}

	rdeps := &worker.WaitGroup{}
	rdeps.AddChild(e.allScheduled)
	for _, parent := range parents {
		rdeps.AddChild(e.targetAnalysisDeps.Get(parent.FQN))
		rdeps.AddChild(e.targetDeps.Get(parent.FQN))
	}

	j, err := e.ScheduleTarget(ctx, e.rrs.Get(target), rdeps)
	if err != nil {
		return err
	}

	e.targetDeps.Get(target.FQN).Add(j)

	return nil
}

func (e *schedEngine) scheduleCacheWarm(ctx context.Context, target *Target, outputs []string) error {
	outputs = append(outputs, inputHashName)

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

	deps := &worker.WaitGroup{}
	for _, child := range children {
		wg := e.targetAnalysisDeps.Get(child.FQN)
		deps.AddChild(wg)
	}

	pmj, err := e.schedulePullMeta(ctx, target)
	if err != nil {
		return err
	}
	deps.Add(pmj)

	j := e.Pool.Schedule(ctx, &worker.Job{
		Name: "analysis " + target.FQN,
		Deps: deps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			defer e.targetAnalysisDeps.Get(target.FQN).DoneSem()

			if e.needRun.Has(target) {
				if e.isSkip(target) {
					// Will get manually ran
					return nil
				}

				err := e.scheduleRun(ctx, target)
				if err != nil {
					return err
				}
				return nil
			}

			cacheToWarm := utils.Filter(e.needCacheWarm.Slice(), func(t TargetWithOutput) bool {
				if t.Target.FQN == target.FQN {
					return true
				}
				return false
			})
			if len(cacheToWarm) > 0 {
				outputs := sets.NewSet(func(s string) string {
					return s
				}, 0)
				for _, c := range cacheToWarm {
					outputs.Add(c.Output)
				}

				j, err := e.ScheduleTargetCacheWarm(ctx, target, outputs.Slice(), nil)
				if err != nil {
					return err
				}
				e.targetDeps.Get(target.FQN).Add(j)
				return nil
			}

			return nil
		},
	})

	taDeps := e.targetAnalysisDeps.Get(target.FQN)
	taDeps.Add(j)

	e.targetDeps.Get(target.FQN).AddChild(taDeps)
	return nil
}

func (e *schedEngine) schedulePullMeta(ctx context.Context, target *Target) (*worker.Job, error) {
	parents, err := e.DAG().GetParents(target)
	if err != nil {
		return nil, err
	}

	pmdeps := &worker.WaitGroup{}
	pmdeps.AddChild(e.allScheduled)
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
			if !hasParentCacheMiss && target.Cache.Enabled && !rr.NoCache {
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
						for _, output := range target.OutWithSupport.Names() {
							e.needCacheWarm.Add(TargetWithOutput{
								Target: target,
								Output: output,
							})
						}
					}
					return nil
				}
			}

			for _, parent := range parents {
				// TODO: only required outputs
				for _, output := range parent.OutWithSupport.Names() {
					e.needCacheWarm.Add(TargetWithOutput{
						Target: parent,
						Output: output,
					})
				}
			}
			e.needRun.Add(target)

			return nil
		},
	})
	e.pullMetaDeps.Get(target.FQN).Add(j)

	return j, nil
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
		err := e.scheduleAnalysis(ctx, target)
		if err != nil {
			return nil, err
		}
	}

	return e.targetDeps, nil
}
