package worker2

import (
	"context"
	"sync"
)

type Value interface {
	Get() (any, error)
}

type MemoryValue[T any] struct {
	V T
}

func (v MemoryValue[T]) Get() (any, error) {
	return v.V, nil
}

type MapValue map[string]Value

func (m MapValue) Get() (any, error) {
	out := map[string]any{}
	for k, vv := range m {
		v, err := vv.Get()
		if err != nil {
			return nil, err
		}
		out[k] = v
	}

	return out, nil
}

func (m MapValue) Set(k string, v Value) {
	m[k] = v
}

type Base struct {
	frozen bool
	m      sync.Mutex
}

func (g *Base) Frozen() bool {
	return g.frozen
}

func (g *Base) Freeze() {
	g.m.Lock()
	defer g.m.Unlock()

	g.frozen = true
}

type Action struct {
	Base
	Deps  []Dep
	Hooks []Hook
	Do    func(ctx context.Context, ins InStore, outs OutStore) error
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
	Base
	Deps  []Dep
	Hooks []Hook
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

func (g *Group) Exec(ctx context.Context, ins InStore, outs OutStore) error {
	ins.Copy(outs)
	return nil
}

type Dep interface {
	Exec(ctx context.Context, ins InStore, outs OutStore) error
	Freeze()
	Frozen() bool
	DirectDeps() []Dep
	GetHooks() []Hook
}

type Named struct {
	Name string
	Dep
}
