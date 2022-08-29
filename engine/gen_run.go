package engine

import (
	"context"
	"fmt"
	log "github.com/sirupsen/logrus"
	"heph/utils"
	"heph/worker"
	"path/filepath"
	"time"
)

type runGenEngine struct {
	*Engine
	pool *worker.Pool
	wg   utils.WaitGroupChan
}

func (e *Engine) ScheduleGenPass(pool *worker.Pool) error {
	if e.ranGenPass {
		return nil
	}

	genTargets := e.GeneratedTargets()

	if len(genTargets) == 0 {
		log.Debugf("No gen targets, skip gen pass")

		linkStartTime := time.Now()
		err := e.linkTargets(false, nil)
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

	log.Debugf("Run gen pass")

	ge := runGenEngine{
		Engine: e,
		pool:   pool,
		wg:     utils.WaitGroupChan{},
	}

	err := ge.linkAndDagGenTargets()
	if err != nil {
		return err
	}

	for _, target := range genTargets {
		err := ge.ScheduleGeneratedPipeline(target)
		if err != nil {
			return err
		}
	}

	pool.Schedule(&worker.Job{
		ID: "finalize gen",
		Wait: func(ctx context.Context) {
			select {
			case <-ctx.Done():
			case <-ge.wg.Done():
			}
		},
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

	return nil
}

func (e *runGenEngine) ScheduleGeneratedPipeline(target *Target) error {
	if !target.Gen {
		panic(fmt.Errorf("%v is not a gen target", target.FQN))
	}

	_, err := e.ScheduleTargetDeps(e.pool, target)
	if err != nil {
		return err
	}

	err = e.ScheduleTarget(e.pool, target)
	if err != nil {
		return err
	}

	err = e.ScheduleRunGenerated(target)
	if err != nil {
		return err
	}

	return nil
}

func (e *runGenEngine) ScheduleRunGenerated(target *Target) error {
	ancestors, err := e.DAG().GetAncestors(target)
	if err != nil {
		return err
	}

	deps := append(ancestors, target)

	log.Tracef("Scheduling rungen %v", target.FQN)

	e.pool.ScheduleWith(
		worker.ScheduleOptions{
			OnSchedule: func() {
				e.wg.Add()
			},
		},
		&worker.Job{
			ID: "rungen-" + target.FQN,
			Wait: func(ctx context.Context) {
				select {
				case <-ctx.Done():
				case <-deps.WaitAllRan():
				}
				return
			},
			Do: func(w *worker.Worker, ctx context.Context) (ferr error) {
				w.Status(fmt.Sprintf("Run generated targets from %v...", target.FQN))
				defer func() {
					w.Status(fmt.Sprintf("Run generated targets %v done: %v", target.FQN, ferr))
				}()

				err := e.runGenerated(target)
				if err != nil {
					return TargetFailedError{
						Target: target,
						Err:    err,
					}
				}

				e.wg.Sub()

				return nil
			},
		})

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

func (e *runGenEngine) runGenerated(target *Target) error {
	log.Tracef("run generated %v", target.FQN)

	start := time.Now()
	defer func() {
		log.Debugf("runGenerated %v took %v", target.FQN, time.Since(start))
	}()

	targets := make(Targets, 0)

	files := target.ActualFilesOut()

	for _, file := range files {
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

				targets = append(targets, t)
				e.Targets = append(e.Targets, t)

				return nil
			},
		}

		_, err := re.runBuildFile(file.Abs())
		if err != nil {
			return fmt.Errorf("%v: %w", file.Abs(), err)
		}
	}

	log.Tracef("run generated got %v targets", len(targets))

	genTargets := make(Targets, 0)
	for _, t := range targets {
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

		for _, t := range genTargets {
			err := e.ScheduleGeneratedPipeline(t)
			if err != nil {
				return err
			}
		}
	}

	return nil
}
