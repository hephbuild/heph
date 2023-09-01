package scheduler

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/hbuiltin"
	"github.com/hephbuild/heph/lcache"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/packages"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/status"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/worker"
	"path/filepath"
	"time"
)

type runGenScheduler struct {
	Name string
	deps *worker.WaitGroup
	*Scheduler
}

func (e *Scheduler) ScheduleGenPass(ctx context.Context, linkAll bool) (_ *worker.WaitGroup, rerr error) {
	if e.RanGenPass {
		return &worker.WaitGroup{}, nil
	}

	genTargets := e.Graph.GeneratedTargets()

	if len(genTargets) == 0 {
		log.Debugf("No gen targets, skip gen pass")

		if linkAll {
			err := e.Graph.LinkTargets(ctx, false, nil)
			if err != nil {
				return nil, fmt.Errorf("linking %w", err)
			}
		}

		return &worker.WaitGroup{}, nil
	}

	ctx, span := e.Observability.SpanGenPass(ctx)
	defer func() {
		if rerr != nil {
			span.EndError(rerr)
		}
	}()

	log.Debugf("Run gen pass")

	ge := runGenScheduler{
		Name:      "Main",
		Scheduler: e,
		deps:      &worker.WaitGroup{},
	}

	err := ge.linkGenTargets(ctx)
	if err != nil {
		return nil, err
	}

	err = ge.ScheduleGeneratedPipeline(ctx, genTargets)
	if err != nil {
		return nil, err
	}

	j := e.Pool.Schedule(ctx, &worker.Job{
		Name: "finalize gen",
		Deps: ge.deps,
		Hook: worker.StageHook{
			OnEnd: func(job *worker.Job) context.Context {
				span.EndError(job.Err())
				return nil
			},
		},
		Do: func(w *worker.Worker, ctx context.Context) error {
			status.Emit(ctx, status.String("Finalizing gen..."))

			// free references to starlark
			for _, p := range e.Packages.All() {
				p.Globals = nil
			}

			if linkAll {
				status.Emit(ctx, status.String("Linking targets..."))

				err := e.Graph.LinkTargets(ctx, false, nil)
				if err != nil {
					return err
				}
			}

			return nil
		},
	})

	e.RanGenPass = true

	deps := worker.WaitGroupJob(j)

	return deps, nil
}

func (e *runGenScheduler) ScheduleGeneratedPipeline(ctx context.Context, targets []*graph.Target) error {
	for _, target := range targets {
		if !target.Gen {
			panic(fmt.Errorf("%v is not a gen target", target.Addr))
		}
	}

	start := time.Now()

	sdeps, err := e.ScheduleTargetsWithDeps(ctx, targets, true, nil)
	if err != nil {
		return err
	}

	newTargets := graph.NewTargets(0)
	deps := &worker.WaitGroup{}
	for _, target := range targets {
		e.scheduleRunGenerated(ctx, target, sdeps.Get(target.Addr), deps, newTargets)
	}

	j := e.Pool.Schedule(ctx, &worker.Job{
		Name: "ScheduleGeneratedPipeline " + e.Name,
		Deps: deps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			status.Emit(ctx, status.String(fmt.Sprintf("Finalizing generated %v...", e.Name)))

			log.Tracef("run generated %v got %v targets in %v", e.Name, newTargets.Len(), time.Since(start))

			genTargets := make([]*graph.Target, 0)
			for _, t := range newTargets.Slice() {
				if t.Gen {
					genTargets = append(genTargets, t)
				}
			}

			if len(genTargets) > 0 {
				err := e.linkGenTargets(ctx)
				if err != nil {
					return err
				}

				err = e.ScheduleGeneratedPipeline(ctx, genTargets)
				if err != nil {
					return err
				}
			}

			return nil
		},
	})
	e.deps.Add(j)

	return nil
}

func (e *Scheduler) linkGenTargets(ctx context.Context) error {
	linkStartTime := time.Now()
	err := e.Graph.LinkTargets(ctx, false, e.Graph.GeneratedTargets())
	if err != nil {
		return fmt.Errorf("linking %w", err)
	}
	log.Debugf("LinkTargets took %v", time.Since(linkStartTime))

	return nil
}

func (e *runGenScheduler) scheduleRunGenerated(ctx context.Context, target *graph.Target, runDeps *worker.WaitGroup, deps *worker.WaitGroup, targets *graph.Targets) {
	j := e.Pool.Schedule(ctx, &worker.Job{
		Name: "rungen_" + target.Addr,
		Deps: runDeps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			ltarget := e.LocalCache.Metas.Find(target)

			return e.scheduleRunGeneratedFiles(ctx, ltarget, deps, targets)
		},
	})
	deps.Add(j)
}

func (e *runGenScheduler) scheduleRunGeneratedFiles(ctx context.Context, target *lcache.Target, deps *worker.WaitGroup, targets *graph.Targets) error {
	files := target.ActualOutFiles().All().WithRoot(target.OutExpansionRoot().Abs())

	chunks := ads.Chunk(files, len(e.Pool.Workers))

	for i, files := range chunks {
		files := files

		j := e.Pool.Schedule(ctx, &worker.Job{
			Name: fmt.Sprintf("rungen %v chunk %v", target.Addr, i),
			Do: func(w *worker.Worker, ctx context.Context) error {
				opts := hbuiltin.Bootstrap(hbuiltin.Opts{
					Pkgs:   e.Packages,
					Root:   e.Root,
					Config: e.Config.Config,
					RegisterTarget: func(spec specs.Target) error {
						err := e.Graph.Register(spec)
						if err != nil {
							return err
						}

						targets.Add(e.Graph.Targets().FindT(spec))
						return nil
					},
				})

				for _, file := range files {
					status.Emit(ctx, status.String(fmt.Sprintf("Running %v", file.RelRoot())))

					ppath := filepath.Dir(file.RelRoot())
					pkg := e.Packages.GetOrCreate(packages.Package{
						Path: ppath,
						Root: xfs.NewPath(e.Root.Root.Abs(), ppath),
					})

					err := e.BuildFilesState.RunBuildFile(pkg, file.Abs(), opts)
					if err != nil {
						return fmt.Errorf("runbuild %v: %w", file.Abs(), err)
					}
				}

				return nil
			},
		})
		deps.Add(j)
	}

	return nil
}
