package engine

import (
	"context"
	"fmt"
	log "github.com/sirupsen/logrus"
	"heph/worker"
	"path/filepath"
	"time"
)

type runGenEngine struct {
	Name string
	deps *worker.WaitGroup
	*Engine
}

func (e *Engine) ScheduleGenPass() (*worker.WaitGroup, error) {
	if e.ranGenPass {
		return &worker.WaitGroup{}, nil
	}

	genTargets := e.GeneratedTargets()

	if len(genTargets) == 0 {
		log.Debugf("No gen targets, skip gen pass")

		linkStartTime := time.Now()
		err := e.linkTargets(false, nil)
		if err != nil {
			return nil, fmt.Errorf("linking %w", err)
		}
		log.Debugf("LinkTargets took %v", time.Since(linkStartTime))

		err = e.createDag()
		if err != nil {
			return nil, err
		}

		return &worker.WaitGroup{}, nil
	}

	log.Debugf("Run gen pass")

	ge := runGenEngine{
		Name:   "Main",
		Engine: e,
		deps:   &worker.WaitGroup{},
	}

	err := ge.linkAndDagGenTargets()
	if err != nil {
		return nil, err
	}

	err = ge.ScheduleGeneratedPipeline(genTargets)
	if err != nil {
		return nil, err
	}

	j := e.Pool.Schedule(&worker.Job{
		ID:   "finalize gen",
		Deps: ge.deps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			w.Status("Finalizing gen...")

			err := e.Simplify()
			if err != nil {
				return err
			}

			err = e.createDag()
			if err != nil {
				return err
			}

			return nil
		},
	})

	e.ranGenPass = true

	deps := &worker.WaitGroup{}
	deps.Add(j)

	return deps, nil
}

func (e *runGenEngine) ScheduleGeneratedPipeline(targets []*Target) error {
	for _, target := range targets {
		if !target.Gen {
			panic(fmt.Errorf("%v is not a gen target", target.FQN))
		}
	}

	start := time.Now()

	sdeps, err := e.ScheduleTargetsWithDeps(targets, nil)
	if err != nil {
		return err
	}

	newTargets := NewTargets(0)
	deps := &worker.WaitGroup{}
	for _, target := range targets {
		e.scheduleRunGenerated(target, sdeps[target], deps, newTargets)
	}

	j := e.Pool.Schedule(&worker.Job{
		ID:   fmt.Sprintf("ScheduleGeneratedPipeline_%v", e.Name),
		Deps: deps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			w.Status(fmt.Sprintf("Finalizing generated %v...", e.Name))

			log.Tracef("run generated %v got %v targets in %v", e.Name, newTargets.Len(), time.Since(start))

			genTargets := make([]*Target, 0)
			for _, t := range newTargets.Slice() {
				err := e.processTarget(t)
				if err != nil {
					return fmt.Errorf("process: %v: %w", t.FQN, err)
				}

				if t.Gen {
					genTargets = append(genTargets, t)
				}
			}

			if len(genTargets) > 0 {
				err := e.linkAndDagGenTargets()
				if err != nil {
					return err
				}

				err = e.ScheduleGeneratedPipeline(genTargets)
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

func (e *runGenEngine) linkAndDagGenTargets() error {
	linkStartTime := time.Now()
	err := e.linkTargets(true, e.GeneratedTargets())
	if err != nil {
		return fmt.Errorf("linking %w", err)
	}
	log.Debugf("LinkTargets took %v", time.Since(linkStartTime))

	err = e.createDag()
	if err != nil {
		return err
	}

	return nil
}

func (e *runGenEngine) scheduleRunGenerated(target *Target, runDeps *worker.WaitGroup, deps *worker.WaitGroup, targets *Targets) {
	j := e.Pool.Schedule(&worker.Job{
		ID:   "rungen_" + target.FQN,
		Deps: runDeps,
		Do: func(w *worker.Worker, ctx context.Context) error {
			return e.scheduleRunGeneratedFiles(target, deps, targets)
		},
	})
	deps.Add(j)
}

func (e *runGenEngine) scheduleRunGeneratedFiles(target *Target, deps *worker.WaitGroup, targets *Targets) error {
	files := target.ActualFilesOut()

	for _, file := range files {
		file := file
		j := e.Pool.Schedule(&worker.Job{
			ID: fmt.Sprintf("rungen_%v_%v", target.FQN, file.Abs()),
			Do: func(w *worker.Worker, ctx context.Context) error {
				w.StatusQuiet(fmt.Sprintf("Running %v", file.RelRoot()))

				re := &runBuildEngine{
					Engine: e.Engine,
					pkg:    e.createPkg(filepath.Dir(file.RelRoot())),
					registerTarget: func(spec TargetSpec) error {
						e.TargetsLock.Lock()
						defer e.TargetsLock.Unlock()

						if t := e.Targets.Find(spec.FQN); t != nil {
							if t.Gen {
								return fmt.Errorf("cannot replace gen target")
							}

							if !t.TargetSpec.Equal(spec) {
								return fmt.Errorf("%v is already declared and does not equal the one defined in %v\n%s\n%s", spec.FQN, t.Source, t.json(), spec.json())
							}

							return nil
						}

						t := &Target{
							TargetSpec: spec,
						}

						targets.Add(t)
						e.Targets.Add(t)

						return nil
					},
				}

				_, err := re.runBuildFile(file.Abs())
				if err != nil {
					return fmt.Errorf("%v: %w", file.Abs(), err)
				}

				return nil
			},
		})
		deps.Add(j)
	}

	return nil
}
