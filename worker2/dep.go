package worker2

import (
	"context"
	"sync"
	"time"
)

type Dep interface {
	GetName() string
	Exec(ctx context.Context, ins InStore, outs OutStore) error
	GetDepsObj() *Deps
	AddDep(...Dep)
	GetHooks() []Hook
	Wait() <-chan struct{}
	DeepDo(f func(Dep))
	GetCtx() context.Context
	SetCtx(ctx context.Context)
	GetErr() error
	GetState() ExecState
	GetScheduledAt() time.Time
	GetStartedAt() time.Time
	GetQueuedAt() time.Time

	setExecution(*Execution)
	getExecution() *Execution
	getMutex() sync.Locker
	GetScheduler() Scheduler
	GetRequest() map[string]float64
}

type baseDep struct {
	execution *Execution
	m         sync.RWMutex

	executionPresentCh chan struct{}
	o                  sync.Once
}

func (a *baseDep) init() {
	if a.executionPresentCh == nil {
		a.executionPresentCh = make(chan struct{})
	}
}

func (a *baseDep) setExecution(e *Execution) {
	a.o.Do(a.init)

	if a.execution != nil {
		if a.execution != e {
			panic("trying to assign different execution to a Dep")
		}
		return
	}

	a.execution = e
	close(a.executionPresentCh)
}

func (a *baseDep) getExecution() *Execution {
	a.o.Do(a.init)

	return a.execution
}

func (a *baseDep) Wait() <-chan struct{} {
	a.o.Do(a.init)

	if exec := a.execution; exec != nil {
		return exec.Wait()
	}

	// Allow to wait Dep that is not scheduled yet

	doneCh := make(chan struct{})

	go func() {
		<-a.executionPresentCh
		<-a.execution.Wait()
		close(doneCh)
	}()

	return doneCh
}

func (a *baseDep) getMutex() sync.Locker {
	return &a.m
}

func (a *baseDep) GetErr() error {
	exec := a.execution
	if exec == nil {
		return nil
	}

	return exec.Err
}

func (a *baseDep) GetState() ExecState {
	exec := a.execution
	if exec == nil {
		return ExecStateUnknown
	}

	return exec.State
}

func (a *baseDep) GetScheduledAt() time.Time {
	exec := a.execution
	if exec == nil {
		return time.Time{}
	}

	return exec.ScheduledAt
}

func (a *baseDep) GetStartedAt() time.Time {
	exec := a.execution
	if exec == nil {
		return time.Time{}
	}

	return exec.StartedAt
}

func (a *baseDep) GetQueuedAt() time.Time {
	exec := a.execution
	if exec == nil {
		return time.Time{}
	}

	return exec.QueuedAt
}

type ActionConfig struct {
	Ctx       context.Context
	Name      string
	Deps      []Dep
	Hooks     []Hook
	Scheduler Scheduler
	Requests  map[string]float64
	Do        func(ctx context.Context, ins InStore, outs OutStore) error
}

type Action struct {
	baseDep
	ctx       context.Context
	name      string
	deps      *Deps
	hooks     []Hook
	scheduler Scheduler
	requests  map[string]float64
	do        func(ctx context.Context, ins InStore, outs OutStore) error
}

func (a *Action) GetScheduler() Scheduler {
	return a.scheduler
}

func (a *Action) GetRequest() map[string]float64 {
	return a.requests
}

func (a *Action) GetName() string {
	return a.name
}

func (a *Action) GetCtx() context.Context {
	if ctx := a.ctx; ctx != nil {
		return ctx
	}
	return context.Background()
}

func (a *Action) SetCtx(ctx context.Context) {
	a.ctx = ctx
}

func (a *Action) OutputCh() <-chan Value {
	h, ch := OutputHook()
	a.hooks = append(a.hooks, h)
	return ch
}

func (a *Action) ErrorCh() <-chan error {
	h, ch := ErrorHook()
	a.hooks = append(a.hooks, h)
	return ch
}

func (a *Action) GetHooks() []Hook {
	return a.hooks
}

func (a *Action) Exec(ctx context.Context, ins InStore, outs OutStore) error {
	if a.do == nil {
		return nil
	}
	return a.do(ctx, ins, outs)
}

func (a *Action) GetDepsObj() *Deps {
	return a.deps
}

func (a *Action) AddDep(deps ...Dep) {
	a.GetDepsObj().Add(deps...)
}

func (a *Action) DeepDo(f func(Dep)) {
	deepDo(a, f)
}

type GroupConfig struct {
	Name string
	Deps []Dep
}

type Group struct {
	baseDep
	name string
	deps *Deps
}

func (g *Group) GetScheduler() Scheduler { return nil }

func (g *Group) GetName() string {
	return g.name
}

func (g *Group) GetDepsObj() *Deps {
	return g.deps
}

func (g *Group) GetHooks() []Hook {
	return nil
}

func (g *Group) DeepDo(f func(Dep)) {
	deepDo(g, f)
}

func (g *Group) GetRequest() map[string]float64 {
	return nil
}

func (g *Group) SetCtx(ctx context.Context) {
	// TODO
}

func (g *Group) GetCtx() context.Context {
	return context.Background()
}

func (g *Group) AddDep(deps ...Dep) {
	g.GetDepsObj().Add(deps...)
}

func (g *Group) Exec(ctx context.Context, ins InStore, outs OutStore) error {
	e := executionFromContext(ctx)
	outs.Set(MapValue(e.inputs))
	return nil
}

type Named struct {
	Name string
	Dep
}

func flattenNamed(dep Dep) Dep {
	for {
		if ndep, ok := dep.(Named); ok {
			dep = ndep.Dep
		} else {
			return dep
		}
	}
}

func Serial(deps []Dep) Dep {
	out := deps[0]

	for i, dep := range deps {
		if i == 0 {
			continue
		}

		prev := out
		dep.AddDep(prev)
		out = dep
	}

	return out
}

func NewChanDep[T any](ctx context.Context, ch chan T) Dep {
	return NewAction(ActionConfig{
		Ctx: ctx,
		Do: func(ctx context.Context, ins InStore, outs OutStore) error {
			return WaitChan(ctx, ch)
		},
	})
}

func NewSemDep(ctx context.Context) *Sem {
	wg := &sync.WaitGroup{}
	return &Sem{
		Dep: NewAction(ActionConfig{
			Ctx: ctx,
			Do: func(ctx context.Context, ins InStore, outs OutStore) error {
				Wait(ctx, func() {
					wg.Wait()
				})
				return nil
			},
		}),
		wg: wg,
	}
}

type Sem struct {
	Dep
	wg *sync.WaitGroup
}

func (s *Sem) AddSem(delta int) {
	s.wg.Add(delta)
}

func (s *Sem) DoneSem() {
	s.wg.Done()
}
