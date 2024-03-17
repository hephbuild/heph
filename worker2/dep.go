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
	DirectDeps() []Dep
	GetHooks() []Hook

	setExecution(*Execution)
	getExecution() *Execution
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

func (a *baseDep) Frozen() bool {
	return a.frozen
}

type Action struct {
	baseDep
	ID    string
	Deps  []Dep
	Hooks []Hook
	Do    func(ctx context.Context, ins InStore, outs OutStore) error
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

func (a *Action) DirectDeps() []Dep {
	return a.Deps
}

type Group struct {
	baseDep
	ID    string
	Deps  []Dep
	Hooks []Hook
}

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

func (g *Group) DirectDeps() []Dep {
	return g.Deps
}

func (g *Group) GetHooks() []Hook {
	return g.Hooks
}

func (g *Group) Add(deps ...Dep) {
	g.m.Lock()
	defer g.m.Unlock()

	if g.frozen {
		panic("group is frozen")
	}

	g.Deps = append(g.Deps, deps...)
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
