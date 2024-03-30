package worker2

import (
	"context"
	"errors"
	"github.com/hephbuild/heph/status"
)

type Worker struct {
	ctx    context.Context
	status status.Statuser
	exec   *Execution
}

func (w *Worker) Status(status status.Statuser) {
	w.status = status
}

func (w *Worker) Interactive() bool {
	return true
}

func (w *Worker) Run() {
	ctx := contextWithExecution(w.ctx, w.exec)
	ctx = status.ContextWithHandler(ctx, w)
	err := w.exec.Run(ctx)
	w.status = nil
	w.exec.scheduler.Done(w.exec.Dep)

	if errors.Is(err, ErrSuspended) {
		w.exec.eventsCh <- EventSuspended{Execution: w.exec}

		go func() {
			select {
			case <-ctx.Done():
				w.exec.eventsCh <- EventCompleted{
					Execution: w.exec,
					Output:    w.exec.outStore.Get(),
					Error:     ctx.Err(),
				}
			case <-w.exec.resumeCh:
				w.exec.eventsCh <- EventReady{Execution: w.exec}
			}
		}()
	} else {
		w.exec.eventsCh <- EventCompleted{
			Execution: w.exec,
			Output:    w.exec.outStore.Get(),
			Error:     err,
		}
	}
}
