package worker2

import (
	"context"
	"errors"
	"slices"
	"sync"
	"time"
)

type Engine struct {
	wg                sync.WaitGroup
	workerProviders   []WorkerProvider
	m                 sync.RWMutex
	c                 *sync.Cond
	executions        []*Execution
	executionsWaiting []*Execution
	eventsCh          chan Event
	hooks             []Hook
}

func NewEngine() *Engine {
	return &Engine{
		eventsCh: make(chan Event, 1000),
		c:        sync.NewCond(&sync.Mutex{}),
	}
}

func (e *Engine) loop() {
	for event := range e.eventsCh {
		e.handle(event)
	}
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
		startedOne := e.tryExecuteOne()
		if !startedOne && len(e.executionsWaiting) > 0 {
			//if e.allWorkersIdle() {
			//	panic(fmt.Errorf("all workers idling, dealock detected"))
			//}

			// retry
			// todo: figure out how to remove the need for that
			time.AfterFunc(100*time.Millisecond, func() {
				e.eventsCh <- event
			})
		}
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

	exec.worker = nil
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
		var deepDeps []Dep
		exec.Dep.DeepDo(func(dep Dep) {
			if dep == exec.Dep {
				return
			}

			deepDeps = append(deepDeps, dep)
		})

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

	exec.Dep.Freeze()

	exec.m.Lock()
	ins := map[string]Value{}
	for _, dep := range exec.Dep.GetDeps() {
		if dep, ok := dep.(Named); ok {
			exec := e.executionForDep(dep.Dep)

			vv := exec.outStore.Get()

			ins[dep.Name] = vv
		}
	}
	exec.inputs = ins
	exec.State = ExecStateWaiting
	exec.m.Unlock()

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
		err := e.start(candidate)
		if err != nil {
			if errors.Is(err, ErrNoWorkerAvail) {
				continue
			}
			e.notifyCompleted(candidate, nil, err)
			continue
		}
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

func (e *Engine) start(exec *Execution) error {
	var errs error
	for _, wp := range e.workerProviders {
		w, err := wp.Start(exec)
		if err != nil {
			if errors.Is(err, ErrNoWorkerAvail) {
				continue
			}
			errs = errors.Join(errs, err)
			continue
		}
		exec.worker = w
		exec.State = ExecStateRunning
		return nil
	}

	// TODO: log errs

	return ErrNoWorkerAvail
}

func (e *Engine) RegisterWorkerProvider(wp WorkerProvider) {
	e.workerProviders = append(e.workerProviders, wp)
}

func (e *Engine) RegisterHook(hook Hook) {
	e.hooks = append(e.hooks, hook)
}

func (e *Engine) Run(ctx context.Context) {
	e.loop()
}

func (e *Engine) Wait() {
	e.wg.Wait()
}

func (e *Engine) allWorkersIdle() bool {
	for _, provider := range e.workerProviders {
		for _, w := range provider.Workers() {
			if w.State() != WorkerStateIdle {
				return false
			}
		}
	}

	return true
}

func (e *Engine) executionForDep(dep Dep) *Execution {
	dep = flattenNamed(dep)

	if e := dep.getExecution(); e != nil {
		return e
	}

	e.m.RLock()
	for _, exec := range e.executions {
		if exec.Dep == dep {
			e.m.RUnlock()
			dep.setExecution(exec)
			return exec
		}
	}
	e.m.RUnlock()

	e.m.Lock()
	defer e.m.Unlock()

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
		worker: nil,
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
