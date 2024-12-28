package hsingleflight

import (
	"golang.org/x/sync/singleflight"
	"sync"
)

type Group[T any] singleflight.Group

func (g *Group[T]) Do(key string, do func() (T, error)) (T, error, bool) {
	sf := (*singleflight.Group)(g)

	vi, err, shared := sf.Do(key, func() (interface{}, error) {
		v, err := do()
		return v, err
	})

	var v T
	if vv, ok := vi.(T); ok {
		v = vv
	}

	return v, err, shared
}

type GroupMem[T any] struct {
	g  Group[T]
	mu sync.RWMutex
	m  map[string]groupMemEntry[T]
}

type groupMemEntry[T any] struct {
	v   T
	err error
}

func (g *GroupMem[T]) get(key string) (T, error, bool) {
	g.mu.RLock()
	defer g.mu.RUnlock()

	e, ok := g.m[key]
	if !ok {
		var zero T
		return zero, nil, false
	}

	return e.v, e.err, true
}

func (g *GroupMem[T]) set(key string, v T, err error) {
	g.mu.Lock()
	defer g.mu.Unlock()

	if g.m == nil {
		g.m = make(map[string]groupMemEntry[T])
	}

	g.m[key] = groupMemEntry[T]{v: v, err: err}
}

func (g *GroupMem[T]) Do(key string, do func() (T, error)) (T, error, bool) {
	v, err, ok := g.get(key)
	if ok {
		return v, err, false
	}

	return g.g.Do(key, func() (T, error) {
		v, err, ok := g.get(key)
		if ok {
			return v, err
		}

		v, err = do()

		g.set(key, v, err)

		return v, err
	})
}
