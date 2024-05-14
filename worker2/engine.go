package worker2

import (
	"github.com/bep/debounce"
	"github.com/dlsniper/debugger"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/sets"
	"github.com/hephbuild/heph/utils/xcontext"
	"github.com/hephbuild/heph/utils/xerrors"
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
	liveExecs        []*Execution
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

func (e *Engine) GetLiveExecutions() []*Execution {
	e.m.Lock()
	defer e.m.Unlock()

	return e.liveExecs[:]
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
		e.finalize(event, ExecStateSkipped, event.Error)
	case EventCompleted:
		if event.Error != nil {
			e.finalize(event, ExecStateFailed, event.Error)
		} else {
			e.finalize(event, ExecStateSucceeded, nil)
		}
	case EventSuspended:
		e.runHooks(event, event.Execution)
		go func() {
			<-event.Bag.WaitResume()
			e.queue(event.Execution)
		}()
	default:
		if event, ok := event.(EventWithExecution); ok {
			e.runHooks(event, event.getExecution())
		}
	}
}

func (e *Engine) finalize(event EventWithExecution, state ExecState, err error) {
	exec := event.getExecution()

	exec.m.Lock()
	exec.Err = err
	exec.State = state
	e.runHooksWithoutLock(event, exec)
	close(exec.completedCh)
	exec.m.Unlock()

	for _, dep := range exec.Dep.GetNode().Dependees.Values() {
		dexec := e.executionForDep(dep)

		dexec.broadcast()
	}

	e.wg.Done()
}

func (e *Engine) runHooks(event Event, exec *Execution) {
	exec.m.Lock()
	defer exec.m.Unlock()

	e.runHooksWithoutLock(event, exec)
}

func (e *Engine) runHooksWithoutLock(event Event, exec *Execution) {
	for _, hook := range e.hooks {
		hook(event)
	}

	for _, hook := range exec.Dep.GetHooks() {
		hook(event)
	}

	if !event.Replayable() {
		return
	}

	exec.events = append(exec.events, event)
}

func (e *Engine) waitForDeps(exec *Execution) error {
	exec.c.L.Lock()
	defer exec.c.L.Unlock()

	for {
		depObj := exec.Dep.GetNode()

		allDepsDone := true
		for _, dep := range depObj.Dependencies.Values() {
			depExec := e.scheduleOne(dep)

			state := depExec.State

			if !state.IsFinal() {
				allDepsDone = false
			}
		}

		if allDepsDone {
			if ok, err := e.tryFreeze(depObj); ok {
				return err
			}
		}

		exec.c.Wait()
	}
}

func (e *Engine) tryFreeze(depObj *Node[Dep]) (bool, error) {
	depObj.m.Lock() // prevent any deps modification
	defer depObj.m.Unlock()

	errs := sets.NewIdentitySet[error](0)

	for _, dep := range depObj.Dependencies.Values() {
		depExec := dep.getExecution()
		if depExec == nil {
			return false, nil
		}

		state := depExec.State

		if !state.IsFinal() {
			return false, nil
		}

		switch state {
		case ExecStateSkipped, ExecStateFailed:
			for _, err := range multierr.Errors(depExec.Err) {
				if serr, ok := xerrors.As[xcontext.SignalCause](err); ok {
					errs.Add(serr)
				} else {
					if jerr, ok := xerrors.As[Error](err); ok {
						errs.Add(jerr.Root())
					} else {
						errs.Add(Error{
							ID:    depExec.ID,
							State: depExec.State,
							Name:  depExec.Dep.GetName(),
							Err:   err,
						})
					}
				}
			}
		}
	}

	depObj.Freeze()

	if errs.Len() > 0 {
		return true, multierr.Combine(errs.Slice()...)
	}

	return true, nil
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

	e.liveExecs = append(e.liveExecs, exec)

	go func() {
		exec.Run()

		e.m.Lock()
		defer e.m.Unlock()

		e.liveExecs = ads.Remove(e.liveExecs, exec)
	}()

	e.runHooks(EventStarted{Execution: exec}, exec)
}

func (e *Engine) RegisterHook(hook Hook) {
	if hook == nil {
		return
	}
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

	for _, dep := range a.GetNode().Dependencies.Values() {
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
		events:      make([]Event, 5),

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

		e.runHooksWithoutLock(EventScheduled{Execution: exec}, exec)

		go e.waitForDepsAndSchedule(exec)
	}

	return exec
}
