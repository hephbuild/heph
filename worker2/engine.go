package worker2

import (
	"context"
	"errors"
	"runtime"
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

func (e *Engine) deepDeps(a Dep, m map[Dep]struct{}, deps *[]Dep) {
	if m == nil {
		m = map[Dep]struct{}{}
	}

	for _, dep := range a.DirectDeps() {
		dep := flattenNamed(dep)

		if _, ok := m[dep]; ok {
			continue
		}
		m[dep] = struct{}{}

		e.deepDeps(dep, m, deps)

		*deps = append(*deps, dep)
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
}

func (e *Engine) runHooks(event Event, exec *Execution) {
	for _, hook := range e.hooks {
		hook(event)
	}

	for _, hook := range exec.Dep.GetHooks() {
		hook(event)
	}
}

func (e *Engine) waitForDeps(exec *Execution, execCache map[Dep]*Execution) bool {
	e.c.L.Lock()
	defer e.c.L.Unlock()

	for {
		var deepDeps []Dep
		e.deepDeps(exec.Dep, nil, &deepDeps)

		allDepsSucceeded := true
		for _, dep := range deepDeps {
			depExec := e.mustExecutionForDep(dep, execCache)

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
			break
		}

		e.c.Wait()
	}

	return true
}

func (e *Engine) waitForDepsAndSchedule(exec *Execution) {
	execCache := map[Dep]*Execution{}

	shouldRun := e.waitForDeps(exec, execCache)
	if !shouldRun {
		return
	}

	exec.Dep.Freeze()

	ins := &inStore{m: map[string]Value{}}
	for _, dep := range exec.Dep.DirectDeps() {
		if dep, ok := dep.(Named); ok {
			exec := e.mustExecutionForDep(dep.Dep, execCache)

			ins.m[dep.Name] = exec.outStore.Get()
		}
	}

	exec.m.Lock()
	exec.inStore = ins
	exec.State = ExecStateWaiting
	exec.m.Unlock()

	e.notifyReady(exec)
}

func (e *Engine) notifySkipped(exec *Execution) {
	e.eventsCh <- EventSkipped{
		Execution: exec,
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
			panic(err)
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
	for _, wp := range e.workerProviders {
		w, err := wp.Start(exec)
		if err != nil {
			if errors.Is(err, ErrNoWorkerAvail) {
				continue
			}
			panic(err)
		}
		exec.worker = w
		exec.State = ExecStateRunning
		return nil
	}

	return ErrNoWorkerAvail
}

func (e *Engine) RegisterWorkerProvider(wp WorkerProvider) {
	e.workerProviders = append(e.workerProviders, wp)
}

func (e *Engine) RegisterHook(hook Hook) {
	e.hooks = append(e.hooks, hook)
}

func (e *Engine) Run(ctx context.Context) {
	e.RegisterWorkerProvider(NewGoroutineWorkerProvider(ctx, runtime.NumCPU()))

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

func (e *Engine) mustExecutionForDep(dep Dep, c map[Dep]*Execution) *Execution {
	dep = flattenNamed(dep)

	if exec, ok := c[dep]; ok {
		return exec
	}

	e.m.RLock()
	for _, exec := range e.executions {
		if exec.Dep == dep {
			e.m.RUnlock()
			c[dep] = exec
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
	e.deepDeps(a, nil, &deps)
	deps = append(deps, a)

	e.m.Lock()
	defer e.m.Unlock()

	for _, dep := range deps {
		_ = e.scheduleOne(dep)
	}
}

func (e *Engine) scheduleOne(dep Dep) *Execution {
	dep = flattenNamed(dep)
	for _, exec := range e.executions {
		if exec.Dep == dep {
			return exec
		}
	}

	exec := &Execution{
		Dep:      dep,
		State:    ExecStateScheduled,
		outStore: &outStore{},
		eventsCh: e.eventsCh,

		// see field comments
		worker:  nil,
		errCh:   nil,
		inStore: nil,
	}
	e.executions = append(e.executions, exec)
	e.wg.Add(1)
	e.runHooks(EventScheduled{Execution: exec}, exec)

	go e.waitForDepsAndSchedule(exec)

	return exec
}
