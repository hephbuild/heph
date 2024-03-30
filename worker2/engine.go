package worker2

import (
	"github.com/bep/debounce"
	"slices"
	"sync"
	"time"
)

type Engine struct {
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
		e.finalize(event.Execution, ExecStateSkipped)
		e.runHooks(event, event.Execution)
	case EventCompleted:
		if event.Error != nil {
			e.finalize(event.Execution, ExecStateFailed)
		} else {
			e.finalize(event.Execution, ExecStateSucceeded)
		}
		e.runHooks(event, event.Execution)
	default:
		if event, ok := event.(WithExecution); ok {
			defer e.runHooks(event, event.getExecution())
		}
	}
}

func (e *Engine) finalize(exec *Execution, state ExecState) {
	exec.m.Lock()
	exec.State = state
	e.wg.Done()
	close(exec.completedCh)
	exec.m.Unlock()

	for _, dep := range exec.Dep.GetDepsObj().Dependees() {
		dexec := dep.owner.getExecution()

		dexec.broadcast()
	}
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
	exec.c.L.Lock()
	defer exec.c.L.Unlock()

	for {
		deepDeps := exec.Dep.GetDepsObj().TransitiveDependencies()

		allDepsSucceeded := true
		for _, dep := range deepDeps {
			depExec := e.executionForDep(dep)

			depExec.m.Lock()
			state := depExec.State
			depExec.m.Unlock()

			if state != ExecStateSucceeded {
				allDepsSucceeded = false
			}

			switch state {
			case ExecStateSkipped, ExecStateFailed:
				return false
			}
		}

		if allDepsSucceeded {
			exec.Dep.Freeze()

			return true
		}

		exec.c.Wait()
	}
}

func (e *Engine) waitForDepsAndSchedule(exec *Execution) {
	shouldRun := e.waitForDeps(exec)
	if !shouldRun {
		e.notifySkipped(exec)
		return
	}

	exec.m.Lock()
	ins := map[string]Value{}
	for _, dep := range exec.Dep.GetDepsObj().Dependencies() {
		if dep, ok := dep.(Named); ok {
			exec := e.executionForDep(dep.Dep)

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

func (e *Engine) notifyReady(exec *Execution) {
	e.eventsCh <- EventReady{
		Execution: exec,
	}
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

	e.runHooks(EventStarted{Execution: exec}, exec)
}

func (e *Engine) RegisterHook(hook Hook) {
	e.hooks = append(e.hooks, hook)
}

func (e *Engine) Run() {
	e.loop()
}

func (e *Engine) Stop() {
	close(e.eventsCh)
}

func (e *Engine) Wait() {
	e.wg.Wait()
}

func (e *Engine) executionForDep(dep Dep) *Execution {
	dep = flattenNamed(dep)

	if e := dep.getExecution(); e != nil {
		return e
	}

	e.m.Lock()
	defer e.m.Unlock()

	return e.scheduleOne(dep)
}

func (e *Engine) Schedule(a Dep) {
	deps := a.GetDepsObj().TransitiveDependencies()
	deps = append(deps, a)

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
	debounceBroadcast := debounce.New(time.Millisecond)
	exec.broadcast = func() {
		debounceBroadcast(func() {
			exec.c.L.Lock()
			exec.c.Broadcast()
			exec.c.L.Unlock()
		})
	}
	exec.c = sync.NewCond(&exec.m)
	// force deps registration
	_ = dep.GetDepsObj()
	dep.setExecution(exec)
	e.wg.Add(1)
	e.runHooks(EventScheduled{Execution: exec}, exec)

	go e.waitForDepsAndSchedule(exec)

	return exec
}
