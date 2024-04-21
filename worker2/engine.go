package worker2

import (
	"github.com/bep/debounce"
	"github.com/dlsniper/debugger"
	"github.com/hephbuild/heph/utils/ads"
	"go.uber.org/multierr"
	"runtime"
	"sync"
	"sync/atomic"
	"time"
)

type Engine struct {
	execUid          uint64
	wg               sync.WaitGroup
	defaultScheduler Scheduler
	workers          []*Worker
	m                sync.RWMutex
	eventsCh         chan Event
	hooks            []Hook
}

func NewEngine() *Engine {
	e := &Engine{
		eventsCh:         make(chan Event, 1000),
		defaultScheduler: UnlimitedScheduler{},
	}

	return e
}

func (e *Engine) SetDefaultScheduler(s Scheduler) {
	e.defaultScheduler = s
}

func (e *Engine) GetWorkers() []*Worker {
	e.m.Lock()
	defer e.m.Unlock()

	return e.workers[:]
}

func (e *Engine) loop() {
	for event := range e.eventsCh {
		e.handle(event)
	}
}

func (e *Engine) handle(event Event) {
	switch event := event.(type) {
	case EventReady:
		e.runHooks(event, event.Execution)
		e.start(event.Execution)
	case EventSkipped:
		e.finalize(event.Execution, ExecStateSkipped, event.Error)
		e.runHooks(event, event.Execution)
	case EventCompleted:
		if event.Error != nil {
			e.finalize(event.Execution, ExecStateFailed, event.Error)
		} else {
			e.finalize(event.Execution, ExecStateSucceeded, nil)
		}
		e.runHooks(event, event.Execution)
	default:
		if event, ok := event.(WithExecution); ok {
			defer e.runHooks(event, event.getExecution())
		}
	}
}

func (e *Engine) finalize(exec *Execution, state ExecState, err error) {
	exec.m.Lock()
	exec.Err = err
	exec.State = state
	close(exec.completedCh)
	exec.m.Unlock()

	for _, dep := range exec.Dep.GetNode().Dependees() {
		dexec := e.executionForDep(dep)

		dexec.broadcast()
	}

	e.wg.Done()
}

func (e *Engine) runHooks(event Event, exec *Execution) {
	for _, hook := range e.hooks {
		if hook == nil {
			continue
		}
		hook(event)
	}

	for _, hook := range exec.Dep.GetHooks() {
		if hook == nil {
			continue
		}
		hook(event)
	}
}

func (e *Engine) waitForDeps(exec *Execution) error {
	exec.c.L.Lock()
	defer exec.c.L.Unlock()

	var errs []error
	for {
		depObj := exec.Dep.GetNode()

		allDepsSucceeded := true
		allDepsDone := true
		errs = errs[:0]
		for _, dep := range depObj.Dependencies() {
			depExec := e.scheduleOne(dep)

			depExec.m.Lock()
			state := depExec.State
			depExec.m.Unlock()

			if !state.IsFinal() {
				allDepsDone = false
			}

			if state != ExecStateSucceeded {
				allDepsSucceeded = false
			}

			switch state {
			case ExecStateSkipped, ExecStateFailed:
				errs = append(errs, Error{
					ID:    depExec.ID,
					State: depExec.State,
					Name:  depExec.Dep.GetName(),
					Err:   depExec.Err,
				})
			}
		}

		if allDepsDone {
			if len(errs) > 0 {
				return multierr.Combine(errs...)
			}

			if allDepsSucceeded {
				if e.tryFreeze(depObj) {
					return nil
				}
			}
		}

		exec.c.Wait()
	}
}

func (e *Engine) tryFreeze(depObj *Node[Dep]) bool {
	depObj.m.Lock() // prevent any deps modification
	defer depObj.m.Unlock()

	for _, dep := range depObj.Dependencies() {
		if dep.GetState() != ExecStateSucceeded {
			return false
		}
	}

	depObj.Freeze()
	return true
}

func (e *Engine) waitForDepsAndSchedule(exec *Execution) {
	debugger.SetLabels(func() []string {
		return []string{
			"where", "waitForDepsAndSchedule",
			"dep_id", exec.Dep.GetName(),
		}
	})

	err := e.waitForDeps(exec)
	if err != nil {
		e.notifySkipped(exec, err)
		return
	}

	exec.m.Lock()
	ins := make(map[string]Value, len(exec.Dep.getNamed()))
	for name, dep := range exec.Dep.getNamed() {
		exec := e.executionForDep(dep)

		vv := exec.outStore.Get()

		ins[name] = vv
	}
	exec.inputs = ins
	exec.State = ExecStateQueued
	exec.QueuedAt = time.Now()
	exec.scheduler = exec.Dep.GetScheduler()
	if exec.scheduler == nil {
		if _, ok := exec.Dep.(*Group); ok {
			// TODO: change to properly use ResourceScheduler
			exec.scheduler = groupScheduler
		} else {
			exec.scheduler = e.defaultScheduler
		}
	}
	exec.m.Unlock()

	e.queue(exec)
}

var groupScheduler = NewLimitScheduler(runtime.NumCPU())

func (e *Engine) queue(exec *Execution) {
	e.notifyQueued(exec)

	err := exec.scheduler.Schedule(exec.Dep, nil) // TODO: pass a way for the scheduler to write into the input
	if err != nil {
		e.notifyCompleted(exec, nil, err)
		return
	}

	e.notifyReady(exec)
}

func (e *Engine) notifySkipped(exec *Execution, err error) {
	e.eventsCh <- EventSkipped{
		At:        time.Now(),
		Error:     err,
		Execution: exec,
	}
}

func (e *Engine) notifyCompleted(exec *Execution, output Value, err error) {
	e.eventsCh <- EventCompleted{
		At:        time.Now(),
		Execution: exec,
		Output:    output,
		Error:     err,
	}
}

func (e *Engine) notifyReady(exec *Execution) {
	e.eventsCh <- EventReady{
		At:        time.Now(),
		Execution: exec,
	}
}

func (e *Engine) notifyQueued(exec *Execution) {
	e.eventsCh <- EventQueued{
		At:        time.Now(),
		Execution: exec,
	}
}

func (e *Engine) start(exec *Execution) {
	e.m.Lock()
	defer e.m.Unlock()

	w := &Worker{
		ctx:  exec.Dep.GetCtx(),
		exec: exec,
		queue: func() {
			e.queue(exec)
		},
	}
	e.workers = append(e.workers, w)

	go func() {
		w.Run()

		e.m.Lock()
		defer e.m.Unlock()

		e.workers = ads.Filter(e.workers, func(worker *Worker) bool {
			return worker != w
		})
	}()

	e.runHooks(EventStarted{Execution: exec}, exec)
}

func (e *Engine) RegisterHook(hook Hook) {
	e.hooks = append(e.hooks, hook)
}

func (e *Engine) Run() {
	debugger.SetLabels(func() []string {
		return []string{
			"where", "worker2.Engine.Run",
		}
	})

	e.loop()
}

func (e *Engine) Stop() {
	close(e.eventsCh)
}

func (e *Engine) Wait() <-chan struct{} {
	ch := make(chan struct{})
	go func() {
		e.wg.Wait()
		close(ch)
	}()

	return ch
}

func (e *Engine) executionForDep(dep Dep) *Execution {
	if exec := dep.getExecution(); exec != nil {
		return exec
	}

	return e.registerOne(dep, true)
}

func (e *Engine) Schedule(a Dep) Dep {
	if !a.GetScheduledAt().IsZero() {
		return nil
	}

	for _, dep := range a.GetNode().Dependencies() {
		_ = e.Schedule(dep)
	}

	_ = e.scheduleOne(a)

	return a
}

func (e *Engine) registerOne(dep Dep, lock bool) *Execution {
	m := dep.getMutex()
	if lock {
		m.Lock()
		defer m.Unlock()
	}

	if exec := dep.getExecution(); exec != nil {
		return exec
	}

	exec := &Execution{
		ID:          atomic.AddUint64(&e.execUid, 1),
		Dep:         dep,
		outStore:    &outStore{},
		eventsCh:    e.eventsCh,
		completedCh: make(chan struct{}),
		m:           m,

		// see field comments
		errCh:  nil,
		inputs: nil,
	}
	debounceBroadcast := debounce.New(time.Millisecond)
	exec.broadcast = func() {
		debounceBroadcast(func() {
			debugger.SetLabels(func() []string {
				return []string{
					"where", "debounceBroadcast",
				}
			})

			exec.c.L.Lock()
			exec.c.Broadcast()
			exec.c.L.Unlock()
		})
	}
	exec.c = sync.NewCond(exec.m)
	//exec.c = sync.NewCond(&exec.Dep.GetNode().m)
	dep.setExecution(exec)

	return exec
}

func (e *Engine) scheduleOne(dep Dep) *Execution {
	m := dep.getMutex()
	m.Lock()
	defer m.Unlock()

	exec := e.registerOne(dep, false)

	if exec.ScheduledAt.IsZero() {
		e.wg.Add(1)

		exec.ScheduledAt = time.Now()
		exec.State = ExecStateScheduled

		e.runHooks(EventScheduled{Execution: exec}, exec)

		go e.waitForDepsAndSchedule(exec)
	}

	return exec
}
