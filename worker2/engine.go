package worker2

import (
	"slices"
	"sync"
	"time"
)

type Engine struct {
	wg                sync.WaitGroup
	defaultScheduler  Scheduler
	workers           []*Worker
	m                 sync.RWMutex
	c                 *sync.Cond
	executions        []*Execution
	executionsWaiting []*Execution
	eventsCh          chan Event
	hooks             []Hook
}

func NewEngine() *Engine {
	e := &Engine{
		eventsCh:         make(chan Event, 1000),
		defaultScheduler: UnlimitedScheduler{},
	}
	e.c = sync.NewCond(&e.m)

	return e
}

func (e *Engine) GetWorkers() []*Worker {
	return e.workers
}

func (e *Engine) loop() {
	done := false
	go func() {
		for {
			time.Sleep(100 * time.Millisecond)
			e.c.Broadcast()
			e.eventsCh <- EventTryExecuteOne{}

			if done {
				return
			}
		}
	}()

	for event := range e.eventsCh {
		e.handle(event)
	}

	done = true
}

func (e *Engine) handle(event Event) {
	if event, ok := event.(WithExecution); ok {
		defer e.runHooks(event, event.getExecution())
	}

	switch event := event.(type) {
	case EventReady:
		e.executionsWaiting = append(e.executionsWaiting, event.Execution)
		go e.notifyTryExecuteOne()
	case EventWorkerAvailable:
		go e.notifyTryExecuteOne()
	case EventTryExecuteOne:
		_ = e.tryExecuteOne()
	case EventSkipped:
		e.finalize(event.Execution, ExecStateSkipped)
	case EventCompleted:
		if event.Error != nil {
			e.finalize(event.Execution, ExecStateFailed)
		} else {
			e.finalize(event.Execution, ExecStateSucceeded)
		}
	}
}

func (e *Engine) finalize(exec *Execution, state ExecState) {
	exec.m.Lock()
	defer exec.m.Unlock()

	exec.State = state
	e.deleteExecution(exec)
	e.wg.Done()
	e.c.Broadcast()
	close(exec.completedCh)
}

func (e *Engine) runHooks(event Event, exec *Execution) {
	for _, hook := range e.hooks {
		hook(event)
	}

	for _, hook := range exec.Dep.GetHooks() {
		hook(event)
	}
}

func (e *Engine) waitForDeps(exec *Execution) bool {
	e.c.L.Lock()
	defer e.c.L.Unlock()

	for {
		deepDeps := exec.Dep.GetDepsObj().TransitiveDependencies()

		allDepsSucceeded := true
		for _, dep := range deepDeps {
			depExec := e.executionForDep(dep)

			if depExec.State != ExecStateSucceeded {
				allDepsSucceeded = false
			}

			switch depExec.State {
			case ExecStateSkipped:
				e.notifySkipped(exec)
				return false
			case ExecStateFailed:
				e.notifySkipped(exec)
				return false
			}
		}

		if allDepsSucceeded {
			exec.Dep.Freeze()

			return true
		}

		e.c.Wait()
	}
}

func (e *Engine) waitForDepsAndSchedule(exec *Execution) {
	shouldRun := e.waitForDeps(exec)
	if !shouldRun {
		return
	}

	exec.m.Lock()
	ins := map[string]Value{}
	for _, dep := range exec.Dep.GetDepsObj().Dependencies() {
		if dep, ok := dep.(Named); ok {
			e.m.Lock()
			exec := e.executionForDep(dep.Dep)
			e.m.Unlock()

			vv := exec.outStore.Get()

			ins[dep.Name] = vv
		}
	}
	exec.inputs = ins
	exec.State = ExecStateWaiting
	exec.scheduler = exec.Dep.GetScheduler()
	if exec.scheduler == nil {
		exec.scheduler = e.defaultScheduler
	}
	exec.m.Unlock()

	err := exec.scheduler.Schedule(exec.Dep, nil) // TODO: pass a way for the scheduler to write into the input
	if err != nil {
		e.notifyCompleted(exec, nil, err)
		return
	}

	e.notifyReady(exec)
}

func (e *Engine) notifySkipped(exec *Execution) {
	e.eventsCh <- EventSkipped{
		Execution: exec,
	}
}

func (e *Engine) notifyCompleted(exec *Execution, output Value, err error) {
	e.eventsCh <- EventCompleted{
		Execution: exec,
		Output:    output,
		Error:     err,
	}
}

func (e *Engine) notifyTryExecuteOne() {
	e.eventsCh <- EventTryExecuteOne{}
}

func (e *Engine) notifyReady(exec *Execution) {
	e.eventsCh <- EventReady{
		Execution: exec,
	}
}

func (e *Engine) tryExecuteOne() bool {
	for _, candidate := range e.executionsWaiting {
		e.start(candidate)
		e.deleteExecutionWaiting(candidate)
		e.runHooks(EventStarted{Execution: candidate}, candidate)
		return true
	}

	return false
}

func (e *Engine) deleteExecution(exec *Execution) {
	// TODO: would need to be deleted once noone depends on it
}

func (e *Engine) deleteExecutionWaiting(exec *Execution) {
	e.executionsWaiting = slices.DeleteFunc(e.executionsWaiting, func(e *Execution) bool {
		return e == exec
	})
}

func (e *Engine) start(exec *Execution) {
	e.m.Lock()
	defer e.m.Unlock()

	w := &Worker{
		ctx:  exec.Dep.GetCtx(),
		exec: exec,
	}
	e.workers = append(e.workers, w)

	go func() {
		w.Run()

		e.m.Lock()
		defer e.m.Unlock()

		e.workers = slices.DeleteFunc(e.workers, func(worker *Worker) bool {
			return worker == w
		})
	}()
}

func (e *Engine) RegisterHook(hook Hook) {
	e.hooks = append(e.hooks, hook)
}

func (e *Engine) Run() {
	e.loop()
}

func (e *Engine) Wait() {
	e.wg.Wait()
}

func (e *Engine) executionForDep(dep Dep) *Execution {
	dep = flattenNamed(dep)

	if e := dep.getExecution(); e != nil {
		return e
	}

	return e.scheduleOne(dep)
}

func (e *Engine) Schedule(a Dep) {
	var deps []Dep
	a.DeepDo(func(dep Dep) {
		deps = append(deps, dep)
	})
	slices.Reverse(deps)

	e.m.Lock()
	defer e.m.Unlock()

	for _, dep := range deps {
		_ = e.scheduleOne(dep)
	}
}

func (e *Engine) scheduleOne(dep Dep) *Execution {
	dep = flattenNamed(dep)

	if exec := dep.getExecution(); exec != nil {
		return exec
	}

	for _, exec := range e.executions {
		if exec.Dep == dep {
			return exec
		}
	}

	exec := &Execution{
		Dep:         dep,
		State:       ExecStateScheduled,
		outStore:    &outStore{},
		eventsCh:    e.eventsCh,
		completedCh: make(chan struct{}),

		// see field comments
		errCh:  nil,
		inputs: nil,
	}
	dep.setExecution(exec)
	e.executions = append(e.executions, exec)
	e.wg.Add(1)
	e.runHooks(EventScheduled{Execution: exec}, exec)

	go e.waitForDepsAndSchedule(exec)

	return exec
}
