package worker2

import (
	"context"
	"sync"
)

type Dep interface {
	GetID() string
	Exec(ctx context.Context, ins InStore, outs OutStore) error
	Freeze()
	IsFrozen() bool
	GetDepsObj() *Deps
	AddDep(Dep)
	GetHooks() []Hook
	Wait() <-chan struct{}
	DeepDo(f func(Dep))
	GetCtx() context.Context

	setExecution(*Execution)
	getExecution() *Execution
	GetScheduler() Scheduler
}

type baseDep struct {
	execution *Execution
	m         sync.RWMutex
}

func (a *baseDep) setExecution(e *Execution) {
	a.m.Lock()
	defer a.m.Unlock()
	a.execution = e
}

func (a *baseDep) getExecution() *Execution {
	a.m.RLock()
	defer a.m.RUnlock()
	return a.execution
}

func (a *baseDep) Wait() <-chan struct{} {
	return a.getExecution().Wait()
}

type Action struct {
	baseDep
	Ctx       context.Context
	ID        string
	Deps      *Deps
	Hooks     []Hook
	Scheduler Scheduler
	Do        func(ctx context.Context, ins InStore, outs OutStore) error
}

func (a *Action) GetScheduler() Scheduler {
	return a.Scheduler
}

func (a *Action) IsFrozen() bool {
	return a.GetDepsObj().IsFrozen()
}

func (a *Action) Freeze() {
	a.Deps.Freeze()
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

func (a *Action) GetDepsObj() *Deps {
	if a.Deps == nil {
		a.Deps = NewDeps()
	}
	a.Deps.setOwner(a)
	return a.Deps
}

func (a *Action) AddDep(dep Dep) {
	a.GetDepsObj().Add(dep)
}

func (a *Action) DeepDo(f func(Dep)) {
	deepDo(a, f)
}

func (a *Action) LinkDeps() {
	for _, dep := range a.GetDepsObj().TransitiveDependencies() {
		_ = dep.GetDepsObj()
	}
}

type Group struct {
	baseDep
	ID    string
	Deps  *Deps
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

func (g *Group) LinkDeps() {
	for _, dep := range g.GetDepsObj().TransitiveDependencies() {
		_ = dep.GetDepsObj()
	}
}

func (g *Group) GetDepsObj() *Deps {
	if g.Deps == nil {
		g.Deps = NewDeps()
	}
	g.Deps.setOwner(g)
	return g.Deps
}

func (g *Group) GetHooks() []Hook {
	return g.Hooks
}

func (g *Group) DeepDo(f func(Dep)) {
	deepDo(g, f)
}

func (g *Group) GetCtx() context.Context {
	return context.Background()
}

func (g *Group) AddDep(dep Dep) {
	g.GetDepsObj().Add(dep)
}

func (g *Group) IsFrozen() bool {
	return g.GetDepsObj().IsFrozen()
}

func (g *Group) Freeze() {
	g.GetDepsObj().Freeze()
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
