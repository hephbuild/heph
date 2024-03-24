package worker2

import (
	"context"
)

type Dep interface {
	GetID() string
	Exec(ctx context.Context, ins InStore, outs OutStore) error
	Freeze()
	IsFrozen() bool
	GetDepsObj() *Deps
	GetDependencies() []Dep
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
	return a.Deps
}

func (a *Action) GetDependencies() []Dep {
	return a.GetDepsObj().Dependencies()
}

func (a *Action) AddDep(dep Dep) {
	a.GetDepsObj().Add(dep)
}

func (a *Action) DeepDo(f func(Dep)) {
	deepDo(a, f)
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

func (g *Group) GetDepsObj() *Deps {
	if g.Deps == nil {
		g.Deps = NewDeps()
	}
	return g.Deps
}

func (g *Group) GetDependencies() []Dep {
	return g.Deps.Dependencies()
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
