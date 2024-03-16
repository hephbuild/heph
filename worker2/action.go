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

type Action struct {
	Deps []Dep
	Do   func(ctx context.Context, ins InStore, outs OutStore) error

	frozen bool
}

func (a *Action) Exec(ctx context.Context, ins InStore, outs OutStore) error {
	return a.Do(ctx, ins, outs)
}

func (a *Action) DirectDeps() []Dep {
	return a.Deps
}

func (a *Action) Freeze() {
	a.frozen = true
}

func (a *Action) Frozen() bool {
	return a.frozen
}

type Group struct {
	Deps   []Dep
	m      sync.Mutex
	frozen bool
}

func (g *Group) Add(deps ...Dep) {
	g.m.Lock()
	defer g.m.Unlock()

	if g.frozen {
		panic("group is frozen")
	}

	g.Deps = append(g.Deps, deps...)
}

func (g *Group) Frozen() bool {
	return g.frozen
}

func (g *Group) Freeze() {
	g.m.Lock()
	defer g.m.Unlock()

	g.frozen = true
}

func (g *Group) DirectDeps() []Dep {
	return g.Deps
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
}

type Named struct {
	Name string
	Dep
}
