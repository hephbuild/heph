package engine

import (
	"context"
	"fmt"
	"heph/utils/maps"
	"heph/utils/sets"
	"heph/worker"
)

func (e *Engine) ScheduleV2TargetRRsWithDeps(octx context.Context, rrs TargetRunRequests, skip *Target) (_ *WaitGroupMap, rerr error) {
	targets := rrs.Targets()

	sctx, span := e.SpanScheduleTargetWithDeps(octx, targets)
	defer func() {
		if rerr != nil {
			span.EndError(rerr)
		}
	}()

	targetsSet := NewTargets(len(targets))
	targetsSet.AddAll(targets)

	toAssess, outputs, err := e.DAG().GetOrderedAncestorsWithOutput(e, targets, true)
	if err != nil {
		return nil, err
	}

	for _, target := range targets {
		ss := sets.NewStringSet(len(target.OutWithSupport.All()))
		ss.AddAll(target.OutWithSupport.Names())
		outputs.Set(target.FQN, ss)
	}

	deps := &WaitGroupMap{}
	pullMetaDeps := &WaitGroupMap{}

	targetSchedLock := &maps.KMutex{}
	targetSchedJobs := &maps.Map[string, *worker.Job]{}

	sched := &schedulerv2{
		Engine:     e,
		octx:       octx,
		sctx:       sctx,
		rrs:        rrs,
		skip:       skip,
		targets:    targets,
		targetsSet: targetsSet,

		toAssess:     toAssess,
		outputs:      outputs,
		deps:         deps,
		pullMetaDeps: pullMetaDeps,

		targetSchedLock: targetSchedLock,
		targetSchedJobs: targetSchedJobs,
	}

	err = sched.schedule()
	if err != nil {
		return nil, err
	}

	go func() {
		<-sched.deps.All().Done()
		span.EndError(sched.deps.All().Err())
	}()

	return sched.deps, nil
}

type schedulerv2 struct {
	*Engine
	octx context.Context
	sctx context.Context
	rrs  TargetRunRequests
	skip *Target

	targets         []*Target
	targetsSet      *Targets
	toAssess        []*Target
	outputs         *maps.Map[string, *sets.Set[string, string]]
	deps            *WaitGroupMap
	pullMetaDeps    *WaitGroupMap
	targetSchedLock *maps.KMutex
	targetSchedJobs *maps.Map[string, *worker.Job]
}

func (s *schedulerv2) schedule() (rerr error) {
	for _, target := range s.toAssess {
		target := target

		targetDeps := s.deps.Get(target.FQN)
		targetDeps.AddSem()

		s.pullMetaDeps.Get(target.FQN).AddSem()

		parents, err := s.DAG().GetParents(target)
		if err != nil {
			return err
		}

		pmdeps := &worker.WaitGroup{}
		for _, parent := range parents {
			pmdeps.AddChild(s.pullMetaDeps.Get(parent.FQN))
		}

		pj := s.Pool.Schedule(s.sctx, &worker.Job{
			Name: "pull_meta " + target.FQN,
			Deps: pmdeps,
			Do: func(w *worker.Worker, ctx context.Context) error {
				w.Status(TargetStatus(target, "Scheduling analysis..."))

				isSkip := s.skip != nil && s.skip.FQN == target.FQN

				if isSkip {
					d, err := s.ScheduleTargetDepsOnce(ctx, target)
					if err != nil {
						return err
					}
					targetDeps.AddChild(d)
					return nil
				}

				rr := s.rrs.Get(target)
				g, err := s.ScheduleTargetGetCacheOrRunOnce(ctx, target, !rr.NoCache, s.targetsSet.Has(target))
				if err != nil {
					return err
				}
				targetDeps.AddChild(g)

				return nil
			},
		})
		go func() {
			<-pj.Wait()
			targetDeps.DoneSem()
		}()
		targetDeps.Add(pj)

		children, err := s.DAG().GetChildren(target)
		if err != nil {
			return err
		}

		for _, child := range children {
			s.pullMetaDeps.Get(child.FQN).Add(pj)
		}
	}

	for _, target := range s.toAssess {
		s.pullMetaDeps.Get(target.FQN).DoneSem()
	}

	return nil
}

func (s *schedulerv2) parentTargetDeps(target *Target) (*worker.WaitGroup, error) {
	deps := &worker.WaitGroup{}
	parents, err := s.DAG().GetParents(target)
	if err != nil {
		return nil, err
	}
	for _, parent := range parents {
		deps.AddChild(s.deps.Get(parent.FQN))
	}

	return deps, nil
}

func (s *schedulerv2) ScheduleTargetCacheGet(ctx context.Context, target *Target, outputs []string) (*worker.Job, error) {
	deps, err := s.parentTargetDeps(target)
	if err != nil {
		return nil, err
	}

	postDeps := &worker.WaitGroup{}
	j := s.Pool.Schedule(ctx, &worker.Job{
		Name: "cache get " + target.FQN,
		Deps: deps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			e := NewTargetRunEngine(s.Engine, w.Status)

			cached, err := e.pullOrGetCache(ctx, target, outputs, false, false)
			if err != nil {
				return err
			}

			if !cached {
				return fmt.Errorf("%v was supposed to pull cache", target.FQN)
			}

			return nil
		},
	})
	postDeps.Add(j)

	return s.ScheduleTargetPostRunOrWarm(ctx, target, postDeps, outputs), nil
}

func (s *schedulerv2) ScheduleTargetCacheGetOnce(ctx context.Context, target *Target, outputs []string) (*worker.Job, error) {
	lock := s.targetSchedLock.Get(target.FQN)
	lock.Lock()
	defer lock.Unlock()

	if j, ok := s.targetSchedJobs.GetOk(target.FQN); ok {
		return j, nil
	}

	j, err := s.ScheduleTargetCacheGet(ctx, target, outputs)
	if err != nil {
		return nil, err
	}

	children, err := s.DAG().GetChildren(target)
	if err != nil {
		return nil, err
	}

	for _, child := range children {
		s.deps.Get(child.FQN).Add(j)
	}
	s.targetSchedJobs.Set(target.FQN, j)

	return j, nil
}

func (s *schedulerv2) ScheduleTargetDepsOnce(ctx context.Context, target *Target) (*worker.WaitGroup, error) {
	parents, err := s.DAG().GetParents(target)
	if err != nil {
		return nil, err
	}

	runDeps := &worker.WaitGroup{}
	for _, parent := range parents {
		j, err := s.ScheduleTargetGetCacheOrRunOnce(ctx, parent, true, true)
		if err != nil {
			return nil, err
		}
		runDeps.AddChild(j)
	}

	return runDeps, nil
}

func (s *schedulerv2) ScheduleTargetGetCacheOrRunOnce(ctx context.Context, target *Target, useCached, pullIfCached bool) (*worker.WaitGroup, error) {
	deps, err := s.parentTargetDeps(target)
	if err != nil {
		return nil, err
	}

	group := &worker.WaitGroup{}
	j := s.Pool.Schedule(ctx, &worker.Job{
		Name: "get cache or run once " + target.FQN,
		Deps: deps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			if target.Cache.Enabled && useCached {
				outputs := s.outputs.Get(target.FQN).Slice()

				e := NewTargetRunEngine(s.Engine, w.Status)

				cached, err := e.pullOrGetCache(ctx, target, outputs, true, true)
				if err != nil {
					return err
				}

				if cached {
					if pullIfCached {
						j, err := s.ScheduleTargetCacheGetOnce(ctx, target, outputs)
						if err != nil {
							return err
						}
						group.Add(j)
					}
					return nil
				}
			}

			j, err := s.ScheduleTargetRunOnce(ctx, target)
			if err != nil {
				return err
			}
			group.Add(j)

			return nil
		},
	})
	group.Add(j)

	return group, nil
}

func (s *schedulerv2) ScheduleTargetRunOnce(ctx context.Context, target *Target) (*worker.Job, error) {
	lock := s.targetSchedLock.Get(target.FQN)
	lock.Lock()
	defer lock.Unlock()

	if j, ok := s.targetSchedJobs.GetOk(target.FQN); ok {
		return j, nil
	}

	runDeps, err := s.ScheduleTargetDepsOnce(ctx, target)
	if err != nil {
		return nil, err
	}

	deps := &worker.WaitGroup{}
	parents, err := s.DAG().GetParents(target)
	if err != nil {
		return nil, err
	}
	for _, parent := range parents {
		deps.AddChild(s.deps.Get(parent.FQN))
	}
	deps.AddChild(runDeps)

	j, err := s.ScheduleTargetRun(ctx, s.rrs.Get(target), runDeps)
	if err != nil {
		return nil, err
	}

	children, err := s.DAG().GetChildren(target)
	if err != nil {
		return nil, err
	}

	for _, child := range children {
		s.deps.Get(child.FQN).Add(j)
	}
	s.targetSchedJobs.Set(target.FQN, j)

	return j, nil
}
