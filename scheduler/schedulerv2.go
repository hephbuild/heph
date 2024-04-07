package scheduler

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
	"github.com/hephbuild/heph/worker2"
)

func (e *schedulerv2) ScheduleTargetRun(ctx context.Context, rr targetrun.Request, deps worker2.Dep) (worker2.Dep, error) {
	j := &worker2.Action{
		Name: rr.Target.Addr,
		Deps: worker2.NewDeps(deps),
		Ctx:  ctx,
		Hooks: []worker2.Hook{
			e.tracker.Hook(),
			observability.WorkerStageFactory(func(job worker2.Dep) (context.Context, *observability.TargetSpan) {
				return e.Observability.SpanRun(job.GetCtx(), rr.Target.GraphTarget())
			}),
		},
		Do: func(ctx context.Context, ins worker2.InStore, outs worker2.OutStore) error {
			err := e.Run(ctx, rr, sandbox.IOConfig{}, e.tracker)
			if err != nil {
				return targetrun.WrapTargetFailed(err, rr.Target)
			}

			return nil
		},
	}
	e.tracker.Register(j)

	return j, nil
}

func (e *Scheduler) ScheduleTargetRRsWithDeps(octx context.Context, rrs targetrun.Requests, skip []specs.Specer) (*WaitGroupMap, *worker2.RunningTracker, error) {
	targetsSet := rrs.Targets()

	toAssess, outputs, err := e.Graph.DAG().GetOrderedAncestorsWithOutput(targetsSet, true)
	if err != nil {
		return nil, nil, err
	}

	for _, target := range targetsSet.Slice() {
		ss := sets.NewStringSet(len(target.OutWithSupport.All()))
		ss.AddAll(target.OutWithSupport.Names())
		outputs.Set(target.Addr, ss)
	}

	sched := &schedulerv2{
		Scheduler: e,
		octx:      octx,
		sctx:      octx,
		rrs:       rrs,
		skip: ads.Map(skip, func(t specs.Specer) string {
			return t.Spec().Addr
		}),
		rrTargets: targetsSet,

		tracker: worker2.NewRunningTracker(),

		toAssess:     toAssess,
		outputs:      outputs,
		done:         &maps.Map[string, *worker2.Sem]{},
		deps:         &WaitGroupMap{},
		pullMetaDeps: &WaitGroupMap{},

		targetSchedLock:        &maps.KMutex{},
		targetSchedJobs:        &maps.Map[string, worker2.Dep]{},
		getCacheOrRunSchedJobs: &maps.Map[getCacheOrRunRequest, worker2.Dep]{},
	}

	err = sched.schedule()
	if err != nil {
		return nil, nil, err
	}

	return sched.deps, sched.tracker, nil
}

type getCacheOrRunRequest struct {
	addr                                  string
	allowCached, pullIfCached, uncompress bool
}

type schedulerv2 struct {
	*Scheduler
	tracker *worker2.RunningTracker

	octx context.Context
	sctx context.Context
	rrs  targetrun.Requests
	skip []string

	rrTargets              *graph.Targets
	toAssess               *graph.Targets
	outputs                *maps.Map[string, *sets.Set[string, string]]
	done                   *maps.Map[string, *worker2.Sem]
	deps                   *WaitGroupMap
	pullMetaDeps           *WaitGroupMap
	targetSchedLock        *maps.KMutex
	targetSchedJobs        *maps.Map[string, worker2.Dep]
	getCacheOrRunSchedJobs *maps.Map[getCacheOrRunRequest, worker2.Dep]
}

func (s *schedulerv2) ggg(d worker2.Dep, target *graph.Target, ch chan struct{}) worker2.Dep {
	return d
	//children, _ := s.Graph.DAG().GetChildren(target)
	//for _, child := range children {
	//	wg := s.done.Get(child.Addr)
	//	if wg == nil {
	//		return nil
	//	}
	//
	//	wg.DoneSem()
	//}
	//
	//return d
	//return &worker2.Action{
	//	Deps: worker2.NewDeps(d),
	//	Do: func(ctx context.Context, ins worker2.InStore, outs worker2.OutStore) error {
	//		children, err := s.Graph.DAG().GetChildren(target)
	//		if err != nil {
	//			return err
	//		}
	//		for _, child := range children {
	//			a := s.done.Get(child.Addr)
	//			if a == nil {
	//				continue
	//			}
	//			a.DoneSem()
	//		}
	//
	//		return nil
	//	},
	//}
	//return &worker2.Action{
	//	Deps: worker2.NewDeps(d),
	//	Hooks: []worker2.Hook{
	//		worker2.StageHook{
	//			OnEnd: func(dep worker2.Dep) context.Context {
	//				children, _ := s.Graph.DAG().GetChildren(target)
	//				for _, child := range children {
	//					wg := s.done.Get(child.Addr)
	//					if wg == nil {
	//						return nil
	//					}
	//
	//					wg.DoneSem()
	//				}
	//				return nil
	//			},
	//		}.Hook(),
	//	},
	//	Do: func(ctx context.Context, ins worker2.InStore, outs worker2.OutStore) error {
	//		return nil
	//	},
	//}
}
func (s *schedulerv2) schedule() error {
	//var sems []chan struct{}
	//for _, target := range s.toAssess.Slice() {
	//	ch := make(chan struct{})
	//	sems = append(sems, ch)
	//
	//	a := &worker2.Action{
	//		Ctx: s.sctx,
	//		Do: func(ctx context.Context, ins worker2.InStore, outs worker2.OutStore) error {
	//			return worker2.WaitChan(ctx, ch)
	//		},
	//	}
	//
	//	s.pullMetaDeps.Get(target.Addr).AddDep(a)
	//}

	//for _, target := range s.toAssess.Slice() {
	//	sem := worker2.NewSemDep()
	//	sem.AddSem(1)
	//
	//	s.done.Set(target.Addr, sem)
	//	s.deps.Get(target.Addr).AddDep(sem)
	//}

	for _, target := range s.toAssess.Slice() {
		targetDeps := s.deps.Get(target.Addr)

		parents, err := s.Graph.DAG().GetParents(target)
		if err != nil {
			return err
		}

		for _, parent := range parents {
			targetDeps.AddDep(s.deps.Get(parent.Addr))
		}
	}

	for _, target := range s.toAssess.Slice() {
		target := target

		targetDeps := s.deps.Get(target.Addr)

		parents, err := s.Graph.DAG().GetParents(target)
		if err != nil {
			return err
		}

		pmdeps := &worker2.Group{}
		for _, parent := range parents {
			pmdeps.AddDep(s.pullMetaDeps.Get(parent.Addr))
		}

		//doneCh := make(chan struct{})
		//
		//targetDeps.AddDep(&worker2.Action{
		//	Ctx: s.sctx,
		//	Do: func(ctx context.Context, ins worker2.InStore, outs worker2.OutStore) error {
		//		return worker2.WaitChan(ctx, doneCh)
		//	},
		//})

		isSkip := ads.Contains(s.skip, target.Addr)

		pj := &worker2.Action{
			Name:  "pull_meta " + target.Addr,
			Deps:  worker2.NewDeps(pmdeps),
			Ctx:   s.sctx,
			Hooks: []worker2.Hook{s.tracker.Hook()},
			Do: func(ctx context.Context, ins worker2.InStore, outs worker2.OutStore) error {
				status.Emit(ctx, tgt.TargetStatus(target, "Scheduling analysis..."))

				if isSkip {
					d, err := s.ScheduleTargetDepsOnce(ctx, target)
					if err != nil {
						return err
					}
					targetDeps.AddDep(s.ggg(d, target, nil))
					return nil
				}

				rr := s.rrs.Get(target)

				g, err := s.ScheduleTargetGetCacheOrRunOnce(
					ctx, target, !rr.Force && !rr.NoCache,
					rr.PullCache || target.Codegen != specs.CodegenNone,
					false,
				)
				if err != nil {
					return err
				}
				targetDeps.AddDep(s.ggg(g, target, nil))
				return nil
			},
		}
		s.tracker.Register(pj)
		targetDeps.AddDep(pj)

		children, err := s.Graph.DAG().GetChildren(target)
		if err != nil {
			return err
		}

		for _, child := range children {
			s.pullMetaDeps.Get(child.Addr).AddDep(pj)
		}
	}

	for _, dep := range s.deps.List() {
		s.Pool.Schedule(dep)
	}

	//for _, ch := range sems {
	//	close(ch)
	//}

	return nil
}

func (s *schedulerv2) parentTargetDeps(target specs.Specer) (worker2.Dep, error) {
	deps := &worker2.Group{}
	parents, err := s.Graph.DAG().GetParents(target)
	if err != nil {
		return nil, err
	}
	for _, parent := range parents {
		deps.AddDep(s.deps.Get(parent.Addr))
	}

	return deps, nil
}

func (s *schedulerv2) ScheduleTargetCacheGet(ctx context.Context, target *graph.Target, outputs []string, withRestoreCache, uncompress bool) (worker2.Dep, error) {
	deps, err := s.parentTargetDeps(target)
	if err != nil {
		return nil, err
	}

	// TODO: add an observability span: OnPullOrGetCache
	j := &worker2.Action{
		Name:  "cache get " + target.Addr,
		Ctx:   ctx,
		Hooks: []worker2.Hook{s.tracker.Hook()},
		Deps:  worker2.NewDeps(deps),
		Do: func(ctx context.Context, ins worker2.InStore, outs worker2.OutStore) error {
			cached, err := s.pullOrGetCacheAndPost(ctx, target, outputs, withRestoreCache, false, uncompress)
			if err != nil {
				return err
			}

			if !cached {
				return fmt.Errorf("%v was supposed to pull cache", target.Addr)
			}

			return nil
		},
	}
	s.tracker.Register(j)

	return j, nil
}

func (s *schedulerv2) ScheduleTargetCacheGetOnce(ctx context.Context, target *graph.Target, outputs []string, withRestoreCache, uncompress bool) (worker2.Dep, error) {
	lock := s.targetSchedLock.Get(target.Addr)
	lock.Lock()
	defer lock.Unlock()

	if j, ok := s.targetSchedJobs.GetOk(target.Addr); ok {
		return j, nil
	}

	j, err := s.ScheduleTargetCacheGet(ctx, target, outputs, withRestoreCache, uncompress)
	if err != nil {
		return nil, err
	}

	// TODO: figure out if thats really needed...
	//children, err := s.Graph.DAG().GetChildren(target.Target)
	//if err != nil {
	//	return nil, err
	//}
	//
	//for _, child := range children {
	//	s.deps.Get(child.Addr).AddDep(j)
	//}
	s.targetSchedJobs.Set(target.Addr, j)

	return j, nil
}

func (s *schedulerv2) ScheduleTargetDepsOnce(ctx context.Context, target specs.Specer) (worker2.Dep, error) {
	parents, err := s.Graph.DAG().GetParents(target)
	if err != nil {
		return nil, err
	}

	runDeps := &worker2.Group{}
	for _, parent := range parents {
		j, err := s.ScheduleTargetGetCacheOrRunOnce(ctx, parent, true, true, true)
		if err != nil {
			return nil, err
		}
		runDeps.AddDep(j)
	}

	return runDeps, nil
}

func (s *schedulerv2) ScheduleTargetGetCacheOrRunOnce(ctx context.Context, target *graph.Target, allowCached, pullIfCached, uncompress bool) (worker2.Dep, error) {
	l := s.targetSchedLock.Get(target.Addr)
	l.Lock()
	defer l.Unlock()

	k := getCacheOrRunRequest{
		addr:         target.Addr,
		allowCached:  allowCached,
		pullIfCached: pullIfCached,
		uncompress:   uncompress,
	}

	if g, ok := s.getCacheOrRunSchedJobs.GetOk(k); ok {
		return g, nil
	}

	deps, err := s.parentTargetDeps(target)
	if err != nil {
		return nil, err
	}

	group := &worker2.Group{}
	j := &worker2.Action{
		Name:  "get cache or run once " + target.Addr,
		Ctx:   ctx,
		Deps:  worker2.NewDeps(deps),
		Hooks: []worker2.Hook{s.tracker.Hook()},
		Do: func(ctx context.Context, ins worker2.InStore, outs worker2.OutStore) error {
			if target.Cache.Enabled && allowCached {
				outputs := s.outputs.Get(target.Addr).Slice()

				_, cached, err := s.Scheduler.pullOrGetCache(ctx, target, outputs, true, false, true, !pullIfCached, true, uncompress)
				if err != nil {
					return err
				}

				if cached {
					if pullIfCached {
						j, err := s.ScheduleTargetCacheGetOnce(ctx, target, outputs, true, uncompress)
						if err != nil {
							return err
						}
						group.AddDep(j)
					}
					return nil
				}
			}

			j, err := s.ScheduleTargetRunOnce(ctx, target)
			if err != nil {
				return err
			}
			group.AddDep(j)

			return nil
		},
	}
	s.tracker.Register(j)
	group.AddDep(j)

	s.getCacheOrRunSchedJobs.Set(k, group)

	return group, nil
}

func (s *schedulerv2) ScheduleTargetRunOnce(ctx context.Context, target *graph.Target) (worker2.Dep, error) {
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

	// TODO: if RestoreCache, try to download the latest artifacts from its lineage

	j, err := s.ScheduleTargetRun(ctx, s.rrs.Get(target), runDeps)
	if err != nil {
		return nil, err
	}

	children, err := s.Graph.DAG().GetChildren(target)
	if err != nil {
		return nil, err
	}

	for _, child := range children {
		s.deps.Get(child.Addr).AddDep(j)
	}
	s.targetSchedJobs.Set(target.Addr, j)

	return j, nil
}
