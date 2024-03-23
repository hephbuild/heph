package worker2

import (
	"context"
	"errors"
	"github.com/hephbuild/heph/status"
)

type WorkerState int

const (
	WorkerStateUnknown WorkerState = iota
	WorkerStateIdle
	WorkerStateRunning
)

type Worker struct {
	ctx    context.Context
	state  WorkerState
	status status.Statuser
}

func (w *Worker) Status(status status.Statuser) {
	w.status = status
}

func (w *Worker) Interactive() bool {
	return true
}

func (w *Worker) State() WorkerState {
	return w.state
}

func (w *Worker) Run(e *Execution) {
	w.state = WorkerStateRunning
	e.worker = w
	ctx := contextWithExecution(w.ctx, e)
	ctx = status.ContextWithHandler(ctx, w)
	err := e.Start(ctx)
	e.scheduler.Done(e.Dep)
	w.state = WorkerStateIdle

	if errors.Is(err, ErrSuspended) {
		e.eventsCh <- EventSuspended{Execution: e}

		go func() {
			select {
			case <-ctx.Done():
				e.eventsCh <- EventCompleted{
					Execution: e,
					Output:    e.outStore.Get(),
					Error:     ctx.Err(),
				}
			case <-e.resumeCh:
				e.eventsCh <- EventReady{Execution: e}
			}
		}()
	} else {
		e.eventsCh <- EventCompleted{
			Execution: e,
			Output:    e.outStore.Get(),
			Error:     err,
		}
	}
	e.eventsCh <- EventWorkerAvailable{Worker: w}
}
