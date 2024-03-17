package worker2

import (
	"context"
	"errors"
	"fmt"
	"runtime"
	"slices"
	"sync"
	"time"
)

var ErrWorkerNotAvail = errors.New("worker not available")
var ErrNoWorkerAvail = errors.New("no worker available")

type Event any

type WithExecution interface {
	getExecution() *Execution
}

type EventTryExecuteOne struct{}

type EventCompleted struct {
	Execution *Execution
	Output    Value
	Error     error
}

func (e EventCompleted) getExecution() *Execution {
	return e.Execution
}

type EventScheduled struct {
	Execution *Execution
}

func (e EventScheduled) getExecution() *Execution {
	return e.Execution
}

type EventStarted struct {
	Execution *Execution
}

func (e EventStarted) getExecution() *Execution {
	return e.Execution
}

type EventSkipped struct {
	Execution *Execution
}

func (e EventSkipped) getExecution() *Execution {
	return e.Execution
}

type EventWorkerAvailable struct {
	Worker Worker
}

type EventReady struct {
	Execution *Execution
}

func (e EventReady) getExecution() *Execution {
	return e.Execution
}

type InStore interface {
	Copy(OutStore)
	Get(key string) (any, error)
}

type OutStore interface {
	Set(Value)
	Get() Value
}

type inStore struct {
	m map[string]Value
}

func (s *inStore) Copy(outs OutStore) {
	mv := make(MapValue, len(s.m))
	for k, v := range s.m {
		mv.Set(k, v)
	}
	outs.Set(mv)
}

func (s *inStore) Get(name string) (any, error) {
	return s.m[name].Get()
}

type outStore struct {
	value Value
}

func (s *outStore) Set(v Value) {
	s.value = v
}

func (s *outStore) Get() Value {
	return s.value
}

type Worker interface {
	Start(a *Execution) error
	State() WorkerState
}

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
		err := e.Start(g.ctx)
		g.state = WorkerStateIdle
		g.m.Unlock()
		e.Completed(err)
	}()

	return nil
}

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

func NewGoroutineWorkerProvider(ctx context.Context) WorkerProvider {
	wp := &GoroutineWorkerProvider{}
	for i := 0; i < runtime.NumCPU(); i++ {
		wp.workers = append(wp.workers, NewGoroutineWorker(ctx))
	}
	return wp
}

type WorkerProvider interface {
	Start(*Execution) (Worker, error)
	Workers() []Worker
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

type WorkerState int

const (
	WorkerStateUnknown WorkerState = iota
	WorkerStateIdle
	WorkerStateRunning
)

type Execution struct {
	Action   Dep
	State    ExecState
	outStore OutStore
	eventsCh chan Event

	worker  Worker     // gets populated when a worker accepts it
	errCh   chan error // gets populated when exec is called
	inStore InStore    // gets populated when its deps are ready
	m       sync.Mutex
}

func (e *Execution) GetOutput() Value {
	return e.outStore.Get()
}

func (e *Execution) Start(ctx context.Context) error {
	defer func() {
		if r := recover(); r != nil {
			e.errCh <- fmt.Errorf("panic: %v", r)
		}
	}()

	if e.errCh == nil {
		e.errCh = make(chan error)

		go func() {
			e.errCh <- e.Action.Exec(ctx, e.inStore, e.outStore)
		}()
	}

	select {
	// TODO implement suspend
	case err := <-e.errCh:
		return err
	}
}

func (e *Execution) Completed(err error) {
	e.eventsCh <- EventCompleted{
		Execution: e,
		Output:    e.outStore.Get(),
		Error:     err,
	}
}

func (e *Engine) deepDeps(a Dep, m map[Dep]struct{}, deps *[]Dep) {
	if m == nil {
		m = map[Dep]struct{}{}
	}

	for _, dep := range a.DirectDeps() {
		dep := noNamed(dep)

		if _, ok := m[dep]; ok {
			continue
		}
		m[dep] = struct{}{}

		e.deepDeps(dep, m, deps)

		*deps = append(*deps, dep)
	}
}

func noNamed(dep Dep) Dep {
	for {
		if ndep, ok := dep.(Named); ok {
			dep = ndep.Dep
		} else {
			break
		}
	}

	return dep
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

	for _, hook := range exec.Action.GetHooks() {
		hook(event)
	}
}

func (e *Engine) waitForDeps(exec *Execution, execCache map[Dep]*Execution) {
	e.c.L.Lock()
	defer e.c.L.Unlock()

	for {
		var deepDeps []Dep
		e.deepDeps(exec.Action, nil, &deepDeps)

		allDepsSucceeded := true
		for _, dep := range deepDeps {
			depExec := e.mustExecutionForDep(dep, execCache)

			if depExec.State != ExecStateSucceeded {
				allDepsSucceeded = false
			}

			switch depExec.State {
			case ExecStateSkipped:
				e.notifySkipped(exec)
				return
			case ExecStateFailed:
				e.notifySkipped(exec)
				return
			}
		}

		if allDepsSucceeded {
			break
		}

		e.c.Wait()
	}
}

func (e *Engine) waitForDepsAndSchedule(exec *Execution) {
	execCache := map[Dep]*Execution{}

	e.waitForDeps(exec, execCache)

	exec.Action.Freeze()

	ins := &inStore{m: map[string]Value{}}
	for _, dep := range exec.Action.DirectDeps() {
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
	e.RegisterWorkerProvider(NewGoroutineWorkerProvider(ctx))

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
	dep = noNamed(dep)

	if exec, ok := c[dep]; ok {
		return exec
	}

	e.m.RLock()
	for _, exec := range e.executions {
		if exec.Action == dep {
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
	dep = noNamed(dep)
	for _, exec := range e.executions {
		if exec.Action == dep {
			return exec
		}
	}

	exec := &Execution{
		Action:   dep,
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
