package worker2

import (
	"context"
	"fmt"
	"github.com/dlsniper/debugger"
	"github.com/hephbuild/heph/status"
	"runtime/debug"
	"strconv"
	"sync"
	"time"
)

type ExecState int

func (s ExecState) IsFinal() bool {
	return s == ExecStateSucceeded || s == ExecStateFailed || s == ExecStateSkipped
}
func (s ExecState) String() string {
	switch s {
	case ExecStateUnknown:
		return "Unknown"
	case ExecStateScheduled:
		return "Scheduled"
	case ExecStateQueued:
		return "Queued"
	case ExecStateRunning:
		return "Running"
	case ExecStateSucceeded:
		return "Succeeded"
	case ExecStateFailed:
		return "Failed"
	case ExecStateSkipped:
		return "Skipped"
	case ExecStateSuspended:
		return "Suspended"
	}

	return strconv.Itoa(int(s))
}

const (
	ExecStateUnknown ExecState = iota
	ExecStateScheduled
	ExecStateQueued
	ExecStateRunning
	ExecStateSucceeded
	ExecStateFailed
	ExecStateSkipped
	ExecStateSuspended
)

type Execution struct {
	ID        uint64
	Dep       Dep
	State     ExecState
	Err       error
	outStore  OutStore
	eventsCh  chan Event
	c         *sync.Cond
	broadcast func()
	events    []Event

	scheduler Scheduler

	errCh  chan error       // gets populated when exec is called
	inputs map[string]Value // gets populated before marking as ready
	m      *sync.RWMutex

	suspendCh   chan *SuspendBag
	resumeAckCh chan struct{}

	completedCh chan struct{}

	ScheduledAt time.Time
	QueuedAt    time.Time
	StartedAt   time.Time
	CompletedAt time.Time

	status status.Statuser

	debugString string
}

func (e *Execution) String() string {
	if id := e.Dep.GetName(); id != "" {
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

func (e *Execution) Run() {
	e.m.Lock()
	if e.errCh == nil {
		e.errCh = make(chan error)
		e.suspendCh = make(chan *SuspendBag)

		if !e.StartedAt.IsZero() {
			panic("double start detected")
		}

		e.StartedAt = time.Now()

		go func() {
			ctx := e.Dep.GetCtx()
			ctx = contextWithExecution(ctx, e)
			ctx = status.ContextWithHandler(ctx, e)

			err := e.run(ctx)
			e.errCh <- err

			e.CompletedAt = time.Now()
		}()
	} else {
		e.ResumeAck()
		e.State = ExecStateRunning
	}
	e.m.Unlock()

	select {
	case sb := <-e.WaitSuspend():
		e.scheduler.Done(e.Dep)

		e.State = ExecStateSuspended

		e.eventsCh <- EventSuspended{Execution: e, Bag: sb}
	case err := <-e.errCh:
		e.scheduler.Done(e.Dep)

		e.eventsCh <- EventCompleted{
			At:        e.CompletedAt,
			Execution: e,
			Output:    e.outStore.Get(),
			Error:     err,
		}
	}
}

func (e *Execution) run(ctx context.Context) error {
	debugger.SetLabels(func() []string {
		return []string{
			"where", "Execution.run",
			"dep_id", e.Dep.GetName(),
		}
	})

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

	//return e.Dep.Exec(ctx, ins, e.outStore)

	return e.safeExec(ctx, ins)
}

func (e *Execution) safeExec(ctx context.Context, ins InStore) (err error) {
	defer func() {
		if r := recover(); r != nil {
			err = fmt.Errorf("panic: %v\n%s", r, debug.Stack())
		}
	}()

	return e.Dep.Exec(ctx, ins, e.outStore)
}

type SuspendBag struct {
	resumeCh    chan struct{}
	resumeAckCh chan struct{}
}

func (e *Execution) Suspend() *SuspendBag {
	e.m.Lock()
	defer e.m.Unlock()

	// useful if debug poolwait is in use, commented for perf
	//stack := debug.Stack()
	//e.debugString = string(stack)

	if e.State == ExecStateSuspended {
		panic("attempting to suspend an already suspended execution")
	}

	resumeCh := make(chan struct{})
	e.resumeAckCh = make(chan struct{})
	sb := &SuspendBag{
		resumeCh:    resumeCh,
		resumeAckCh: e.resumeAckCh,
	}

	e.suspendCh <- sb

	return sb
}

func (e *SuspendBag) Resume() <-chan struct{} {
	ackCh := e.resumeAckCh

	if ackCh == nil {
		panic("attempting to resume an unsuspended execution")
	}

	close(e.resumeCh)

	return ackCh
}

func (e *SuspendBag) WaitResume() <-chan struct{} {
	return e.resumeCh
}

func (e *Execution) ResumeAck() {
	close(e.resumeAckCh)
}

func (e *Execution) WaitSuspend() <-chan *SuspendBag {
	return e.suspendCh
}

func (e *Execution) Status(status status.Statuser) {
	e.status = status
}

func (e *Execution) GetStatus() status.Statuser {
	s := e.status
	if s == nil {
		s = status.String("")
	}
	return s
}

func (e *Execution) Interactive() bool {
	return true
}
