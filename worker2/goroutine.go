package worker2

import (
	"context"
	"errors"
	"sync"
)

type GoroutineWorker struct {
	m     sync.Mutex
	ctx   context.Context
	state WorkerState
}

func NewGoroutineWorker(ctx context.Context) *GoroutineWorker {
	w := &GoroutineWorker{
		ctx:   ctx,
		state: WorkerStateIdle,
	}
	return w
}

func (g *GoroutineWorker) State() WorkerState {
	return g.state
}

func (g *GoroutineWorker) Start(e *Execution) error {
	ok := g.m.TryLock()
	if !ok {
		return ErrWorkerNotAvail
	}

	go func() {
		g.state = WorkerStateRunning
		ctx := contextWithExecution(g.ctx, e)
		err := e.Start(ctx)
		g.state = WorkerStateIdle
		g.m.Unlock()
		if errors.Is(err, ErrSuspended) {
			go func() {
				<-e.resumeCh
				e.eventsCh <- EventReady{Execution: e}
			}()
		} else {
			e.eventsCh <- EventCompleted{
				Execution: e,
				Output:    e.outStore.Get(),
				Error:     err,
			}
		}
	}()

	return nil
}

func NewGoroutineWorkerProvider(ctx context.Context, n int) WorkerProvider {
	wp := &GoroutineWorkerProvider{}
	for i := 0; i < n; i++ {
		wp.workers = append(wp.workers, NewGoroutineWorker(ctx))
	}
	return wp
}

type GoroutineWorkerProvider struct {
	workers []*GoroutineWorker
}

func (wp *GoroutineWorkerProvider) Workers() []Worker {
	workers := make([]Worker, 0, len(wp.workers))
	for _, worker := range wp.workers {
		workers = append(workers, worker)
	}
	return workers
}

func (wp *GoroutineWorkerProvider) Start(e *Execution) (Worker, error) {
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