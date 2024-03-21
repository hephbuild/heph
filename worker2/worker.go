package worker2

import (
	"context"
	"errors"
	"github.com/hephbuild/heph/status"
	"sync"
)

type Worker struct {
	m      sync.Mutex
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

func NewWorker(ctx context.Context) *Worker {
	w := &Worker{
		ctx:   ctx,
		state: WorkerStateIdle,
	}
	return w
}

func (w *Worker) State() WorkerState {
	return w.state
}

func (w *Worker) Start(e *Execution) error {
	ok := w.m.TryLock()
	if !ok {
		return ErrWorkerNotAvail
	}

	go func() {
		w.state = WorkerStateRunning
		ctx := contextWithExecution(w.ctx, e)
		ctx = status.ContextWithHandler(ctx, w)
		err := e.Start(ctx)
		w.state = WorkerStateIdle
		w.m.Unlock()
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
	}()

	return nil
}

func NewStaticWorkerProvider(ctx context.Context, n int) WorkerProvider {
	wp := &StaticWorkerProvider{}
	for i := 0; i < n; i++ {
		wp.workers = append(wp.workers, NewWorker(ctx))
	}
	return wp
}

type StaticWorkerProvider struct {
	workers []*Worker
	m       sync.Mutex
}

func (wp *StaticWorkerProvider) Workers() []*Worker {
	workers := make([]*Worker, 0, len(wp.workers))
	for _, worker := range wp.workers {
		workers = append(workers, worker)
	}
	return workers
}

func (wp *StaticWorkerProvider) Start(e *Execution) (*Worker, error) {
	for _, w := range wp.workers {
		err := w.Start(e)
		if err != nil {
			if errors.Is(err, ErrWorkerNotAvail) {
				continue
			}
			return nil, err
		}

		return w, nil
	}

	return nil, ErrNoWorkerAvail
}
