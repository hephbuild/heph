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

	worker  Worker     // gets populated when a worker accepts it
	errCh   chan error // gets populated when exec is called
	inStore InStore    // gets populated when its deps are ready
	m       sync.Mutex

	suspendCh   chan struct{}
	resumeCh    chan struct{}
	resumeAckCh chan struct{}
}

func (e *Execution) String() string {
	return e.Dep.GetID()
}

func (e *Execution) GetOutput() Value {
	return e.outStore.Get()
}

var ErrSuspended = errors.New("suspended")

func (e *Execution) Start(ctx context.Context) error {
	e.m.Lock()
	if e.errCh == nil {
		e.errCh = make(chan error)
		e.suspendCh = make(chan struct{})

		go func() {
			err := e.safeExec(ctx)
			e.errCh <- err
		}()
	} else {
		e.resumeAckCh <- struct{}{}
	}
	e.m.Unlock()

	select {
	case <-e.suspendCh:
		return ErrSuspended
	case err := <-e.errCh:
		return err
	}
}

func (e *Execution) safeExec(ctx context.Context) (err error) {
	defer func() {
		if r := recover(); r != nil {
			err = fmt.Errorf("panic: %v", r)
		}
	}()

	return e.Dep.Exec(ctx, e.inStore, e.outStore)
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
