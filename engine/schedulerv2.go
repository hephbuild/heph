package engine

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/observability"
	"github.com/hephbuild/heph/sandbox"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/targetrun"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/maps"
	"github.com/hephbuild/heph/utils/sets"
	"github.com/hephbuild/heph/worker"
)

func (e *Engine) ScheduleTargetRRsWithDeps(ctx context.Context, rrs TargetRunRequests, skip []specs.Specer) (*WaitGroupMap, error) {
	return e.ScheduleV2TargetRRsWithDeps(ctx, rrs, skip)
}

func (e *Engine) ScheduleTargetRun(ctx context.Context, rr targetrun.Request, deps *worker.WaitGroup) (*worker.Job, error) {
	j := e.Pool.Schedule(ctx, &worker.Job{
		Name: rr.Target.Addr,
		Deps: deps,
		Hook: WorkerStageFactory(func(job *worker.Job) (context.Context, *observability.TargetSpan) {
			return e.Observability.SpanRun(job.Ctx(), rr.Target.GraphTarget())
		}),
		Do: func(w *worker.Worker, ctx context.Context) error {
			err := e.Run(ctx, rr, sandbox.IOConfig{})
			if err != nil {
				return targetrun.WrapTargetFailed(err, rr.Target)
			}

			return nil
		},
	})

	return j, nil
}

func (e *Engine) ScheduleV2TargetRRsWithDeps(octx context.Context, rrs TargetRunRequests, skip []specs.Specer) (_ *WaitGroupMap, rerr error) {
	targetsSet := rrs.Targets()

	toAssess, outputs, err := e.Graph.DAG().GetOrderedAncestorsWithOutput(targetsSet, true)
	if err != nil {
		return nil, err
	}

	for _, target := range targetsSet.Slice() {
		ss := sets.NewStringSet(len(target.OutWithSupport.All()))
		ss.AddAll(target.OutWithSupport.Names())
		outputs.Set(target.Addr, ss)
	}

	deps := &WaitGroupMap{}
	pullMetaDeps := &WaitGroupMap{}

	targetSchedLock := &maps.KMutex{}
	targetSchedJobs := &maps.Map[string, *worker.Job]{}

	sched := &schedulerv2{
		Engine: e,
		octx:   octx,
		sctx:   octx,
		rrs:    rrs,
		skip: ads.Map(skip, func(t specs.Specer) string {
			return t.Spec().Addr
		}),
		rrTargets: targetsSet,

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

	return sched.deps, nil
}

type schedulerv2 struct {
	*Engine
	octx context.Context
	sctx context.Context
	rrs  TargetRunRequests
	skip []string

	rrTargets       *graph.Targets
	toAssess        *graph.Targets
	outputs         *maps.Map[string, *sets.Set[string, string]]
	deps            *WaitGroupMap
	pullMetaDeps    *WaitGroupMap
	targetSchedLock *maps.KMutex
	targetSchedJobs *maps.Map[string, *worker.Job]
}

func (s *schedulerv2) schedule() error {
	for _, target := range s.toAssess.Slice() {
		target := target

		targetDeps := s.deps.Get(target.Addr)
		targetDeps.AddSem()

		s.pullMetaDeps.Get(target.Addr).AddSem()

		parents, err := s.Graph.DAG().GetParents(target)
		if err != nil {
			return err
		}

		pmdeps := &worker.WaitGroup{}
		for _, parent := range parents {
			pmdeps.AddChild(s.pullMetaDeps.Get(parent.Addr))
		}

		isSkip := ads.Contains(s.skip, target.Addr)
		isInRRs := s.rrTargets.Has(target)

		pj := s.Pool.Schedule(s.sctx, &worker.Job{
			Name: "pull_meta " + target.Addr,
			Deps: pmdeps,
			Do: func(w *worker.Worker, ctx context.Context) error {
				status.Emit(ctx, tgt.TargetStatus(target, "Scheduling analysis..."))

				if isSkip {
					d, err := s.ScheduleTargetDepsOnce(ctx, target)
					if err != nil {
						return err
					}
					targetDeps.AddChild(d)
					return nil
				}

				rr := s.rrs.Get(target)
				g, err := s.ScheduleTargetGetCacheOrRunOnce(ctx, target, !rr.NoCache, isInRRs, false)
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

		children, err := s.Graph.DAG().GetChildren(target)
		if err != nil {
			return err
		}

		for _, child := range children {
			s.pullMetaDeps.Get(child.Addr).Add(pj)
		}
	}

	for _, target := range s.toAssess.Slice() {
		s.pullMetaDeps.Get(target.Addr).DoneSem()
	}

	return nil
}

func (s *schedulerv2) parentTargetDeps(target specs.Specer) (*worker.WaitGroup, error) {
	deps := &worker.WaitGroup{}
	parents, err := s.Graph.DAG().GetParents(target)
	if err != nil {
		return nil, err
	}
	for _, parent := range parents {
		deps.AddChild(s.deps.Get(parent.Addr))
	}

	return deps, nil
}

func (s *schedulerv2) ScheduleTargetCacheGet(ctx context.Context, target *graph.Target, outputs []string, uncompress bool) (*worker.Job, error) {
	deps, err := s.parentTargetDeps(target)
	if err != nil {
		return nil, err
	}

	// TODO: add an observability span: OnPullOrGetCache
	return s.Pool.Schedule(ctx, &worker.Job{
		Name: "cache get " + target.Addr,
		Deps: deps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			cached, err := s.pullOrGetCacheAndPost(ctx, target, outputs, false, uncompress)
			if err != nil {
				return err
			}

			if !cached {
				return fmt.Errorf("%v was supposed to pull cache", target.Addr)
			}

			return nil
		},
	}), nil
}

func (s *schedulerv2) ScheduleTargetCacheGetOnce(ctx context.Context, target *graph.Target, outputs []string, uncompress bool) (*worker.Job, error) {
	lock := s.targetSchedLock.Get(target.Addr)
	lock.Lock()
	defer lock.Unlock()

	if j, ok := s.targetSchedJobs.GetOk(target.Addr); ok {
		return j, nil
	}

	j, err := s.ScheduleTargetCacheGet(ctx, target, outputs, uncompress)
	if err != nil {
		return nil, err
	}

	children, err := s.Graph.DAG().GetChildren(target.Target)
	if err != nil {
		return nil, err
	}

	for _, child := range children {
		s.deps.Get(child.Addr).Add(j)
	}
	s.targetSchedJobs.Set(target.Addr, j)

	return j, nil
}

func (s *schedulerv2) ScheduleTargetDepsOnce(ctx context.Context, target specs.Specer) (*worker.WaitGroup, error) {
	parents, err := s.Graph.DAG().GetParents(target)
	if err != nil {
		return nil, err
	}

	runDeps := &worker.WaitGroup{}
	for _, parent := range parents {
		j, err := s.ScheduleTargetGetCacheOrRunOnce(ctx, parent, true, true, true)
		if err != nil {
			return nil, err
		}
		runDeps.AddChild(j)
	}

	return runDeps, nil
}

func (s *schedulerv2) ScheduleTargetGetCacheOrRunOnce(ctx context.Context, target *graph.Target, useCached, pullIfCached, uncompress bool) (*worker.WaitGroup, error) {
	deps, err := s.parentTargetDeps(target)
	if err != nil {
		return nil, err
	}

	group := &worker.WaitGroup{}
	j := s.Pool.Schedule(ctx, &worker.Job{
		Name: "get cache or run once " + target.Addr,
		Deps: deps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			if target.Cache.Enabled && useCached {
				outputs := s.outputs.Get(target.Addr).Slice()

				_, cached, err := s.Engine.pullOrGetCache(ctx, target, outputs, true, !pullIfCached, true, uncompress)
				if err != nil {
					return err
				}

				if cached {
					if pullIfCached {
						j, err := s.ScheduleTargetCacheGetOnce(ctx, target, outputs, uncompress)
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

func (s *schedulerv2) ScheduleTargetRunOnce(ctx context.Context, target *graph.Target) (*worker.Job, error) {
	lock := s.targetSchedLock.Get(target.Addr)
	lock.Lock()
	defer lock.Unlock()

	if j, ok := s.targetSchedJobs.GetOk(target.Addr); ok {
		return j, nil
	}

	runDeps, err := s.ScheduleTargetDepsOnce(ctx, target)
	if err != nil {
		return nil, err
	}

	deps := &worker.WaitGroup{}
	parents, err := s.Graph.DAG().GetParents(target)
	if err != nil {
		return nil, err
	}
	for _, parent := range parents {
		deps.AddChild(s.deps.Get(parent.Addr))
	}
	deps.AddChild(runDeps)

	j, err := s.ScheduleTargetRun(ctx, s.rrs.Get(target), runDeps)
	if err != nil {
		return nil, err
	}

	children, err := s.Graph.DAG().GetChildren(target)
	if err != nil {
		return nil, err
	}

	for _, child := range children {
		s.deps.Get(child.Addr).Add(j)
	}
	s.targetSchedJobs.Set(target.Addr, j)

	return j, nil
}
