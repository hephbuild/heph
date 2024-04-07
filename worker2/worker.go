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
	queue  func()
}

func (w *Worker) Status(status status.Statuser) {
	w.status = status
}

func (w *Worker) GetStatus() status.Statuser {
	s := w.status
	if s == nil {
		s = status.String("")
	}
	return s
}

func (w *Worker) Interactive() bool {
	return true
}

func (w *Worker) Execution() *Execution {
	return w.exec
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
				w.queue()
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
