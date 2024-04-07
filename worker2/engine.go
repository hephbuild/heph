package worker2

import (
	"github.com/bep/debounce"
	"github.com/dlsniper/debugger"
	"github.com/hephbuild/heph/utils/ads"
	"go.uber.org/multierr"
	"sync"
	"sync/atomic"
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
	e.wg.Done()
	close(exec.completedCh)
	exec.m.Unlock()

	for _, dep := range exec.Dep.GetDepsObj().Dependees() {
		dexec := e.executionForDep(dep.owner)

		dexec.broadcast()
	}
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

	for {
		depObj := exec.Dep.GetDepsObj()

		allDepsSucceeded := true
		allDepsDone := true
		var errs []error
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
				if _, ok := dep.(*Group); ok && dep.GetName() == "" {
					errs = append(errs, depExec.Err)
				} else {
					errs = append(errs, Error{
						ID:    depExec.ID,
						State: depExec.State,
						Name:  depExec.Dep.GetName(),
						Err:   depExec.Err,
					})
				}
			}
		}

		if allDepsDone {
			if len(errs) > 0 {
				return multierr.Combine(errs...)
			}

			if allDepsSucceeded {
				depObj.Freeze()

				return nil
			}
		}

		exec.c.Wait()
	}
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
	ins := map[string]Value{}
	for _, dep := range exec.Dep.GetDepsObj().Dependencies() {
		if dep, ok := dep.(Named); ok {
			exec := e.executionForDep(dep.Dep)

			vv := exec.outStore.Get()

			ins[dep.Name] = vv
		}
	}
	exec.inputs = ins
	exec.State = ExecStateQueued
	exec.QueuedAt = time.Now()
	exec.scheduler = exec.Dep.GetScheduler()
	if exec.scheduler == nil {
		if _, ok := exec.Dep.(*Group); ok {
			// TODO: change to properly use ResourceScheduler
			exec.scheduler = UnlimitedScheduler{}
		} else {
			exec.scheduler = e.defaultScheduler
		}
	}
	exec.m.Unlock()

	e.queue(exec)
}

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
	dep = flattenNamed(dep)

	return e.registerOne(dep, true)
}

func (e *Engine) Schedule(a Dep) Dep {
	deps := a.GetDepsObj().TransitiveDependencies()

	for _, dep := range deps {
		_ = e.scheduleOne(dep)
	}

	_ = e.scheduleOne(a)

	return a
}

var execUid uint64

func (e *Engine) registerOne(dep Dep, lock bool) *Execution {
	dep = flattenNamed(dep)

	if lock {
		m := dep.getMutex()
		m.Lock()
		defer m.Unlock()
	}

	if exec := dep.getExecution(); exec != nil {
		return exec
	}

	// force deps registration
	_ = dep.GetDepsObj()

	exec := &Execution{
		ID:          atomic.AddUint64(&execUid, 1),
		Dep:         dep,
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
	dep.setExecution(exec)

	return exec
}

func (e *Engine) scheduleOne(dep Dep) *Execution {
	m := dep.getMutex()
	m.Lock()
	defer m.Unlock()

	exec := e.registerOne(dep, false)

	if exec.ScheduledAt.IsZero() {
		exec.ScheduledAt = time.Now()
		exec.State = ExecStateScheduled

		e.wg.Add(1)

		e.runHooks(EventScheduled{Execution: exec}, exec)

		go e.waitForDepsAndSchedule(exec)
	}

	return exec
}
