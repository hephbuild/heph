package worker2

import (
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/utils/ads"
	"runtime"
	"sync"
	"time"
)

var ErrWorkerNotAvail = errors.New("worker not available")
var ErrNoWorkerAvail = errors.New("no worker available")

type Event any

type EventSchedule struct {
	Action Dep
}

type WithExecution interface {
	getExecution() *Execution
}

type EventCompleted struct {
	Execution *Execution
	Output    Value
	Error     error
}

func (e EventCompleted) getExecution() *Execution {
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
	ch    chan *Execution
	ctx   context.Context
	state WorkerState
}

func NewGoroutineWorker(ctx context.Context) *GoroutineWorker {
	w := &GoroutineWorker{
		ch:    make(chan *Execution),
		ctx:   ctx,
		state: WorkerStateIdle,
	}
	go w.Run()
	return w
}

func (g *GoroutineWorker) State() WorkerState {
	return g.state
}

func (g *GoroutineWorker) Run() {
	for e := range g.ch {
		g.state = WorkerStateRunning
		err := e.Exec(g.ctx)
		e.Completed(err)
		g.state = WorkerStateIdle
	}
}

func (g *GoroutineWorker) Start(a *Execution) error {
	select {
	case g.ch <- a:
		return nil
	default:
		return ErrWorkerNotAvail
	}
}

type Engine struct {
	wg                sync.WaitGroup
	workerProviders   []WorkerProvider
	executions        []*Execution
	executionsWaiting []*Execution
	eventsCh          chan Event
	hooks             []Hook
}

func NewEngine() *Engine {
	return &Engine{
		eventsCh: make(chan Event, 1000),
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
}

func (e *Execution) GetOutput() Value {
	return e.outStore.Get()
}

func (e *Execution) Exec(ctx context.Context) error {
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

func (e *Engine) Schedule(a Dep) {
	var deps []Dep
	e.deepDeps(a, nil, &deps)
	for _, dep := range deps {
		e.wg.Add(1)
		e.eventsCh <- EventSchedule{Action: dep}
	}
	e.wg.Add(1)
	e.eventsCh <- EventSchedule{Action: a}
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
	case EventSchedule:
		exec := &Execution{
			Action:   event.Action,
			State:    ExecStateScheduled,
			outStore: &outStore{},
			eventsCh: e.eventsCh,

			// see field comments
			worker:  nil,
			errCh:   nil,
			inStore: nil,
		}
		e.executions = append(e.executions, exec)
		go e.waitForDepsAndSchedule(exec)
		e.runHooks(event, exec)
	case EventWorkerAvailable, EventReady:
		startedOne := e.tryExecuteOne()
		if !startedOne && len(e.executionsWaiting) > 0 && e.allWorkersIdle() {
			panic(fmt.Errorf("all workers idling, dealock detected"))
		}
	case EventSkipped:
		event.Execution.State = ExecStateSkipped
		e.finalize(event.Execution)
	case EventCompleted:
		if event.Error != nil {
			event.Execution.State = ExecStateFailed
		} else {
			event.Execution.State = ExecStateSucceeded
		}
		e.finalize(event.Execution)
	}
}

func (e *Engine) finalize(exec *Execution) {
	exec.worker = nil
	e.deleteExecution(exec)
	e.wg.Done()
}

func (e *Engine) runHooks(event Event, exec *Execution) {
	for _, hook := range e.hooks {
		hook(event)
	}

	for _, hook := range exec.Action.GetHooks() {
		hook(event)
	}
}

func (e *Engine) waitForDepsAndSchedule(exec *Execution) {
	for {
		<-time.After(time.Millisecond) // TODO: replace with broadcast

		var deepDeps []Dep
		e.deepDeps(exec.Action, nil, &deepDeps)

		allDepsSucceeded := true
		for _, dep := range deepDeps {
			depExec := e.executionForDep(dep)

			if depExec.State != ExecStateSucceeded {
				allDepsSucceeded = false
			}

			switch depExec.State {
			case ExecStateSkipped:
				go e.notifySkipped(exec)
				return
			case ExecStateFailed:
				go e.notifySkipped(exec)
				return
			}
		}

		if allDepsSucceeded {
			break
		}
	}

	exec.Action.Freeze()

	ins := &inStore{m: map[string]Value{}}
	for _, dep := range exec.Action.DirectDeps() {
		if dep, ok := dep.(Named); ok {
			exec := e.executionForDep(dep.Dep)

			ins.m[dep.Name] = exec.outStore.Get()
		}
	}
	exec.inStore = ins
	exec.State = ExecStateWaiting

	e.executionsWaiting = append(e.executionsWaiting, exec)
	go e.notifyReady(exec)
}

func (e *Engine) notifySkipped(exec *Execution) {
	e.eventsCh <- EventSkipped{
		Execution: exec,
	}
}

func (e *Engine) notifyReady(exec *Execution) {
	e.eventsCh <- EventReady{
		Execution: exec,
	}
}

func (e *Engine) tryExecuteOne() bool {
	for _, candidate := range e.executionsWaiting {
		_, err := e.start(candidate)
		if err != nil {
			if errors.Is(err, ErrNoWorkerAvail) {
				continue
			}
			panic(err)
		}
		return true
	}

	return false
}

func (e *Engine) deleteExecution(exec *Execution) {
	return // TODO: would need to be deleted once noone depends on it
	e.executions = ads.Filter(e.executions, func(e *Execution) bool {
		return e != exec
	})
}

func (e *Engine) deleteExecutionWaiting(exec *Execution) {
	e.executionsWaiting = ads.Filter(e.executionsWaiting, func(e *Execution) bool {
		return e != exec
	})
}

func (e *Engine) gc() {
	e.executions = ads.Filter(e.executions, func(e *Execution) bool {
		return e.State.IsFinal()
	})
}

func (e *Engine) start(exec *Execution) (Worker, error) {
	for _, wp := range e.workerProviders {
		w, err := wp.Start(exec)
		if err != nil {
			if errors.Is(err, ErrNoWorkerAvail) {
				continue
			}
			panic(err)
		}
		e.deleteExecutionWaiting(exec)
		exec.worker = w
		exec.State = ExecStateRunning
		return w, nil
	}

	return nil, ErrNoWorkerAvail
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

func (e *Engine) executionForDep(dep Dep) *Execution {
	for _, exec := range e.executions {
		if exec.Action == dep {
			return exec
		}
	}

	panic("execution not found")
}
