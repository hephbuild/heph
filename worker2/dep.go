package worker2

import (
	"context"
	"sync"
)

type Dep interface {
	GetID() string
	Exec(ctx context.Context, ins InStore, outs OutStore) error
	Freeze()
	Frozen() bool
	GetDeps() []Dep
	AddDep(Dep)
	GetHooks() []Hook
	Wait() <-chan struct{}
	DeepDo(f func(Dep))
	GetCtx() context.Context

	deepDo(m map[Dep]struct{}, f func(Dep))
	setExecution(*Execution)
	getExecution() *Execution
	GetScheduler() Scheduler
}

type baseDep struct {
	frozen    bool
	m         sync.Mutex
	execution *Execution
}

func (a *baseDep) setExecution(e *Execution) {
	a.execution = e
}

func (a *baseDep) getExecution() *Execution {
	return a.execution
}

func (a *baseDep) Wait() <-chan struct{} {
	return a.getExecution().Wait()
}

func (a *baseDep) Frozen() bool {
	return a.frozen
}

type Action struct {
	baseDep
	Ctx       context.Context
	ID        string
	Deps      []Dep
	Hooks     []Hook
	Scheduler Scheduler
	Do        func(ctx context.Context, ins InStore, outs OutStore) error
}

func (a *Action) GetScheduler() Scheduler {
	return a.Scheduler
}

func (a *Action) Freeze() {
	a.m.Lock()
	defer a.m.Unlock()

	for _, dep := range a.Deps {
		if !dep.Frozen() {
			panic("attempting to free while all deps arent frozen")
		}
	}

	a.frozen = true
}

func (a *Action) GetID() string {
	return a.ID
}

func (a *Action) GetCtx() context.Context {
	if ctx := a.Ctx; ctx != nil {
		return ctx
	}
	return context.Background()
}

func (a *Action) OutputCh() <-chan Value {
	h, ch := OutputHook()
	a.Hooks = append(a.Hooks, h)
	return ch
}

func (a *Action) ErrorCh() <-chan error {
	h, ch := ErrorHook()
	a.Hooks = append(a.Hooks, h)
	return ch
}

func (a *Action) GetHooks() []Hook {
	return a.Hooks
}

func (a *Action) Exec(ctx context.Context, ins InStore, outs OutStore) error {
	return a.Do(ctx, ins, outs)
}

func (a *Action) GetDeps() []Dep {
	return a.Deps
}

func (a *Action) AddDep(dep Dep) {
	a.m.Lock()
	defer a.m.Unlock()

	if a.frozen {
		panic("action is frozen")
	}

	a.Deps = append(a.Deps, dep)
}

func (a *Action) DeepDo(f func(Dep)) {
	m := map[Dep]struct{}{}
	a.deepDo(m, f)
}

func (a *Action) deepDo(m map[Dep]struct{}, f func(Dep)) {
	deepDo(a, m, f, true)
}

type Group struct {
	baseDep
	ID    string
	Deps  []Dep
	Hooks []Hook
}

func (g *Group) GetScheduler() Scheduler { return nil }

func (g *Group) OutputCh() <-chan Value {
	h, ch := OutputHook()
	g.Hooks = append(g.Hooks, h)
	return ch
}

func (g *Group) ErrorCh() <-chan error {
	h, ch := ErrorHook()
	g.Hooks = append(g.Hooks, h)
	return ch
}

func (g *Group) GetID() string {
	return g.ID
}

func (g *Group) GetDeps() []Dep {
	return g.Deps
}

func (g *Group) GetHooks() []Hook {
	return g.Hooks
}

func (g *Group) DeepDo(f func(Dep)) {
	m := map[Dep]struct{}{}
	g.deepDo(m, f)
}

func (g *Group) deepDo(m map[Dep]struct{}, f func(Dep)) {
	deepDo(g, m, f, true)
}

func (g *Group) GetCtx() context.Context {
	return context.Background()
}

func (g *Group) AddDep(dep Dep) {
	g.m.Lock()
	defer g.m.Unlock()

	if g.frozen {
		panic("group is frozen")
	}

	g.Deps = append(g.Deps, dep)
}

func (g *Group) Freeze() {
	g.m.Lock()
	defer g.m.Unlock()

	for _, dep := range g.Deps {
		if !dep.Frozen() {
			panic("attempting to free while all deps arent frozen")
		}
	}

	g.frozen = true
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
			break
		}
	}

	return dep
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
