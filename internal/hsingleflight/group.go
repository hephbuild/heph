package hsingleflight

import (
	"fmt"
	"github.com/dlsniper/debugger"
	"sync"

	"golang.org/x/sync/singleflight"
)

type Group[T any] singleflight.Group

func (g *Group[T]) Do(key string, do func() (T, error)) (T, error, bool) {
	sf := (*singleflight.Group)(g)

	debugger.SetLabels(func() []string {
		return []string{
			fmt.Sprintf("do: outer: %v", key), "",
		}
	})

	var computed bool
	vi, err, _ := sf.Do(key, func() (interface{}, error) {
		debugger.SetLabels(func() []string {
			return []string{
				fmt.Sprintf("do: inner: %v", key), "",
			}
		})

		computed = true

		return do()
	})

	var v T
	if vv, ok := vi.(T); ok {
		v = vv
	}

	return v, err, computed
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

func (g *GroupMem[T]) Set(key string, v T, err error) {
	g.set(key, v, err)
}

func (g *GroupMem[T]) Do(key string, do func() (T, error)) (T, error, bool) {
	v, err, ok := g.get(key)
	if ok {
		return v, err, false
	}

	var computed bool
	v, err, _ = g.g.Do(key, func() (T, error) {
		v, err, ok := g.get(key)
		if ok {
			return v, err
		}

		computed = true

		v, err = do()

		g.set(key, v, err)

		return v, err
	})

	return v, err, computed
}
