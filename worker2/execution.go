package worker2

import (
	"context"
	"errors"
	"fmt"
	"sync"
)

type ExecState int

func (s ExecState) IsFinal() bool {
	return s == ExecStateSucceeded || s == ExecStateFailed || s == ExecStateSkipped
}

const (
	ExecStateUnknown ExecState = iota
	ExecStateScheduled
	ExecStateWaiting
	ExecStateRunning
	ExecStateSucceeded
	ExecStateFailed
	ExecStateSkipped
	ExecStateSuspended
)

type Execution struct {
	Dep      Dep
	State    ExecState
	outStore OutStore
	eventsCh chan Event

	scheduler Scheduler

	errCh  chan error       // gets populated when exec is called
	inputs map[string]Value // gets populated before marking as ready
	m      sync.Mutex

	suspendCh   chan struct{}
	resumeCh    chan struct{}
	resumeAckCh chan struct{}

	completedCh chan struct{}
}

func (e *Execution) String() string {
	if id := e.Dep.GetID(); id != "" {
		return id
	}

	return fmt.Sprintf("%p", e)
}

func (e *Execution) Wait() <-chan struct{} {
	return e.completedCh
}

func (e *Execution) GetOutput() Value {
	return e.outStore.Get()
}

var ErrSuspended = errors.New("suspended")

func (e *Execution) Run(ctx context.Context) error {
	e.m.Lock()
	if e.errCh == nil {
		e.errCh = make(chan error)
		e.suspendCh = make(chan struct{})

		go func() {
			err := e.run(ctx)
			e.errCh <- err
		}()
	} else {
		e.resumeAckCh <- struct{}{}
		e.State = ExecStateRunning
	}
	e.m.Unlock()

	select {
	case <-e.suspendCh:
		e.State = ExecStateSuspended
		return ErrSuspended
	case err := <-e.errCh:
		return err
	}
}

func (e *Execution) run(ctx context.Context) error {
	if g, ok := e.Dep.(*Group); ok {
		return g.Exec(ctx, nil, e.outStore)
	}

	ins := &inStore{m: map[string]any{}}
	for k, value := range e.inputs {
		vv, err := value.Get()
		if err != nil {
			return fmt.Errorf("%v: %w", k, err)
		}

		ins.m[k] = vv
	}

	return e.safeExec(ctx, ins)
}

func (e *Execution) safeExec(ctx context.Context, ins InStore) (err error) {
	defer func() {
		if r := recover(); r != nil {
			err = fmt.Errorf("panic: %v", r)
		}
	}()

	return e.Dep.Exec(ctx, ins, e.outStore)
}

func (e *Execution) Suspend() {
	e.m.Lock()
	defer e.m.Unlock()

	if e.resumeCh != nil {
		panic("attempting to suspend an already suspended execution")
	}

	e.suspendCh <- struct{}{}
	e.resumeCh = make(chan struct{})
	e.resumeAckCh = make(chan struct{})
}

func (e *Execution) Resume() <-chan struct{} {
	e.m.Lock()
	defer e.m.Unlock()

	if e.resumeAckCh == nil {
		panic("attempting to resume an unsuspended execution")
	}

	ackCh := e.resumeAckCh

	e.resumeCh <- struct{}{}
	e.resumeCh = nil

	return ackCh
}
