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
	"github.com/hephbuild/heph/worker2"
	"path/filepath"
	"time"
)

type runGenScheduler struct {
	Name    string
	tracker *worker2.RunningTracker
	*Scheduler
}

func (e *Scheduler) ScheduleGenPass(ctx context.Context, genTargets []*graph.Target) (_ worker2.Dep, rerr error) {
	if len(genTargets) == 0 {
		log.Debugf("No gen targets, skip gen pass")

		return worker2.NewGroup(), nil
	}

	log.Debugf("Run gen pass")

	ge := runGenScheduler{
		Name:      "Main",
		Scheduler: e,
		tracker:   worker2.NewRunningTracker(),
	}

	ctx, span := e.Observability.SpanGenPass(ctx)

	j1 := worker2.NewAction(worker2.ActionConfig{
		Name:  "schedule gen pipeline",
		Ctx:   ctx,
		Hooks: []worker2.Hook{ge.tracker.Hook()},
		Do: func(ctx context.Context, ins worker2.InStore, outs worker2.OutStore) error {
			return ge.ScheduleGeneratedPipeline(ctx, genTargets)
		},
	})

	j := worker2.NewAction(worker2.ActionConfig{
		Name: "finalize gen",
		Ctx:  ctx,
		Hooks: []worker2.Hook{
			worker2.StageHook{
				OnEnd: func(dep worker2.Dep) context.Context {
					span.EndError(dep.GetErr())
					return nil
				},
			}.Hook(),
		},
		Deps: []worker2.Dep{j1, ge.tracker.Group()},
		Do: func(ctx context.Context, ins worker2.InStore, outs worker2.OutStore) error {
			status.Emit(ctx, status.String("Finalizing gen..."))

			return nil
		},
	})

	return j, nil
}

func (e *runGenScheduler) ScheduleGeneratedPipeline(ctx context.Context, targets []*graph.Target) error {
	for _, target := range targets {
		if !target.IsGen() {
			panic(fmt.Errorf("%v is not a gen target", target.Addr))
		}
	}

	status.Emit(ctx, status.String(fmt.Sprintf("Linking targets...")))

	err := e.Graph.LinkTargets(ctx, false, targets, false)
	if err != nil {
		return fmt.Errorf("linking: %w", err)
	}

	start := time.Now()

	status.Emit(ctx, status.String(fmt.Sprintf("Scheduling %v...", e.Name)))

	sdeps, _, err := e.ScheduleTargetsWithDeps(ctx, targets, true, nil)
	if err != nil {
		return err
	}

	newTargets := graph.NewTargets(0)
	deps := worker2.NewGroup()
	for _, target := range targets {
		e.scheduleRunGenerated(ctx, target, sdeps.Get(target.Addr), deps, newTargets)
	}

	e.Pool.Schedule(worker2.NewAction(worker2.ActionConfig{
		Name:  "ScheduleGeneratedPipeline " + e.Name,
		Ctx:   ctx,
		Hooks: []worker2.Hook{e.tracker.Hook()},
		Deps:  []worker2.Dep{deps},
		Do: func(ctx context.Context, ins worker2.InStore, outs worker2.OutStore) error {
			status.Emit(ctx, status.String(fmt.Sprintf("Finalizing generated %v...", e.Name)))

			log.Tracef("run generated %v got %v targets in %v", e.Name, newTargets.Len(), time.Since(start))

			genTargets := ads.Filter(newTargets.Slice(), func(target *graph.Target) bool {
				return target.IsGen()
			})

			if len(genTargets) > 0 {
				err := e.ScheduleGeneratedPipeline(ctx, genTargets)
				if err != nil {
					return err
				}
			}

			return nil
		},
	}))

	return nil
}

func (e *runGenScheduler) scheduleRunGenerated(ctx context.Context, target *graph.Target, runDeps worker2.Dep, deps *worker2.Group, targets *graph.Targets) {
	j := worker2.NewAction(worker2.ActionConfig{
		Name: "rungen_" + target.Addr,
		Deps: []worker2.Dep{runDeps},
		Ctx:  ctx,
		Do: func(ctx context.Context, ins worker2.InStore, outs worker2.OutStore) error {
			ltarget := e.LocalCache.Metas.Find(target)

			return e.scheduleRunGeneratedFiles(ctx, ltarget, deps, targets)
		},
	})
	deps.AddDep(j)
}

type matchGen struct {
	addr     string
	matchers []specs.Matcher
}

func (e *runGenScheduler) scheduleRunGeneratedFiles(ctx context.Context, target *lcache.Target, deps *worker2.Group, targets *graph.Targets) error {
	matchers := []matchGen{{
		addr:     target.Addr,
		matchers: target.Gen,
	}}

	deepGenSources := target.DeepGenSources()
	matchers = ads.GrowExtra(matchers, len(deepGenSources))
	for _, source := range deepGenSources {
		matchers = append(matchers, matchGen{
			addr:     source.Addr,
			matchers: source.Gen,
		})
	}

	files := target.ActualOutFiles().All().WithRoot(target.OutExpansionRoot().Abs())

	opts := hbuiltin.Bootstrap(hbuiltin.Opts{
		Pkgs:   e.Packages,
		Root:   e.Root,
		Config: e.Config,
		RegisterTarget: func(spec specs.Target) error {
			for _, entry := range matchers {
				addrMatchers := ads.Filter(entry.matchers, func(m specs.Matcher) bool {
					return specs.IsAddrMatcher(m)
				})

				addrMatch := ads.Some(addrMatchers, func(m specs.Matcher) bool {
					return m.Match(spec)
				})

				if !addrMatch {
					return fmt.Errorf("%v doest match any gen pattern of %v: %v", spec.Addr, entry.addr, entry.matchers)
				}

				labelMatchers := ads.Filter(entry.matchers, func(m specs.Matcher) bool {
					return specs.IsLabelMatcher(m)
				})

				for _, label := range spec.Labels {
					labelMatch := ads.Some(labelMatchers, func(m specs.Matcher) bool {
						return m.(specs.StringMatcher).MatchString(label)
					})

					if !labelMatch {
						return fmt.Errorf("label `%v` doest match any gen pattern of %v: %v", label, entry.addr, entry.matchers)
					}
				}
			}

			spec.GenSources = []string{target.Addr}

			t, err := e.Graph.Register(spec)
			if err != nil {
				return err
			}

			targets.Add(t)
			return nil
		},
	})

	for _, file := range files {
		file := file
		j := worker2.NewAction(worker2.ActionConfig{
			Name:  fmt.Sprintf("rungen %v file %v", target.Addr, file.RelRoot()),
			Ctx:   ctx,
			Hooks: []worker2.Hook{e.tracker.Hook()},
			Do: func(ctx context.Context, ins worker2.InStore, outs worker2.OutStore) error {
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

				return nil
			},
		})
		deps.AddDep(j)
	}

	return nil
}
