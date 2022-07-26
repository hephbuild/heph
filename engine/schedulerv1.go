package engine

import (
	"context"
	"fmt"
	log "github.com/sirupsen/logrus"
	"heph/worker"
)

func (e *Engine) ScheduleV1TargetRRsWithDeps(octx context.Context, rrs TargetRunRequests, skip *Target) (_ *WaitGroupMap, rerr error) {
	targets := rrs.Targets()

	sctx, span := e.SpanScheduleTargetWithDeps(octx, targets)
	defer func() {
		if rerr != nil {
			span.EndError(rerr)
		}
	}()

	toAssess, err := e.DAG().GetOrderedAncestors(targets, true)
	if err != nil {
		return nil, err
	}

	needRun := NewTargets(0)
	needCacheWarm := NewTargets(0)

	for _, target := range targets {
		if skip != nil && skip.FQN == target.FQN {
			parents, err := e.DAG().GetParents(target)
			if err != nil {
				return nil, err
			}

			needCacheWarm.AddAll(parents)
			needRun.Add(target)
		} else {
			needCacheWarm.Add(target)
		}
	}

	deps := &WaitGroupMap{}
	pullMetaDeps := &WaitGroupMap{}
	pullAllMetaDeps := &worker.WaitGroup{}

	for _, target := range toAssess {
		target := target

		pj := e.Pool.Schedule(sctx, &worker.Job{
			Name: "pull_meta " + target.FQN,
			Deps: pullMetaDeps.Get(target.FQN),
			Do: func(w *worker.Worker, ctx context.Context) error {
				w.Status(fmt.Sprintf("Scheduling analysis %v...", target.FQN))

				parents, err := e.DAG().GetParents(target)
				if err != nil {
					return err
				}

				hasParentCacheMiss := false
				for _, parent := range parents {
					if needRun.Find(parent.FQN) != nil {
						hasParentCacheMiss = true
						break
					}
				}

				rr := rrs.Get(target)
				if !hasParentCacheMiss && target.Cache.Enabled && !rr.NoCache {
					w.Status(fmt.Sprintf("Pulling meta %v...", target.FQN))

					e := TargetRunEngine{
						Engine: e,
						Print:  w.Status,
					}

					cached, err := e.PullTargetMeta(ctx, target, target.OutWithSupport.Names())
					if err != nil {
						return err
					}

					if cached {
						return nil
					}
				}

				needCacheWarm.AddAll(parents)
				needRun.Add(target)

				return nil
			},
		})
		pullAllMetaDeps.Add(pj)

		children, err := e.DAG().GetChildren(target)
		if err != nil {
			return nil, err
		}

		for _, child := range children {
			pullMetaDeps.Get(child.FQN).Add(pj)
		}
	}

	scheduleDeps := &WaitGroupMap{}

	for _, target := range toAssess {
		target := target

		targetDeps := deps.Get(target.FQN)
		targetDeps.AddSem()

		sdeps := &worker.WaitGroup{}
		// TODO: Replace with waiting for all dependants pull meta of target.FQN in the list of all ancestors
		sdeps.AddChild(pullAllMetaDeps)
		sdeps.AddChild(scheduleDeps.Get(target.FQN))

		sj := e.Pool.Schedule(octx, &worker.Job{
			Name: "schedule " + target.FQN,
			Deps: sdeps,
			Do: func(w *worker.Worker, ctx context.Context) error {
				w.Status(fmt.Sprintf("Scheduling %v...", target.FQN))

				parents, err := e.DAG().GetParents(target)
				if err != nil {
					return err
				}

				wdeps := &worker.WaitGroup{}
				for _, parent := range parents {
					pdeps := deps.Get(parent.FQN)
					wdeps.AddChild(pdeps)
				}

				if skip != nil && target.FQN == skip.FQN {
					log.Debugf("%v skip", target.FQN)
					targetDeps.AddChild(wdeps)

					return nil
				}

				if needRun.Find(target.FQN) != nil {
					j, err := e.ScheduleTarget(ctx, rrs.Get(target), wdeps)
					if err != nil {
						return err
					}
					targetDeps.Add(j)
				} else if needCacheWarm.Find(target.FQN) != nil {
					j, err := e.ScheduleTargetCacheWarm(ctx, target, target.OutWithSupport.Names(), wdeps)
					if err != nil {
						return err
					}
					targetDeps.Add(j)
				}

				log.Debugf("%v nothing to do", target.FQN)

				return nil
			},
		})

		children, err := e.DAG().GetChildren(target)
		if err != nil {
			return nil, err
		}

		for _, child := range children {
			scheduleDeps.Get(child.FQN).Add(sj)
		}

		targetDeps.Add(sj)
		targetDeps.DoneSem()
	}

	go func() {
		<-deps.All().Done()
		span.End()
	}()

	return deps, nil
}
