package hiter

import (
	"context"
	"errors"
	"sync"

	"github.com/hephbuild/heph/internal/herrgroup"
)

type Group[K, V any] struct {
	wg          herrgroup.ContextGroup
	yield       func(K, V) bool
	yieldExited bool
	yieldmu     sync.Mutex
}

func NewGroup[K, V any](ctx context.Context, yield func(K, V) bool) *Group[K, V] {
	return &Group[K, V]{
		wg:    herrgroup.NewContext(ctx, true),
		yield: yield,
	}
}

var errYieldExited = errors.New("yield exited")

func (g *Group[K, V]) Go(f func(ctx context.Context, yield func(K, V) bool)) {
	if g.yieldExited {
		return
	}

	g.wg.Go(func(ctx context.Context) error {
		if g.yieldExited {
			return errYieldExited
		}

		f(ctx, g.gyield)

		if g.yieldExited {
			return errYieldExited
		}

		return nil
	})
}

func (g *Group[K, V]) Wait() {
	_ = g.wg.Wait()
}

func (g *Group[K, V]) SetLimit(n int) {
	g.wg.SetLimit(n)
}

func (g *Group[K, V]) gyield(k K, v V) bool {
	g.yieldmu.Lock()
	defer g.yieldmu.Unlock()

	if g.yieldExited {
		return false
	}

	r := g.yield(k, v)
	if !r {
		g.yieldExited = true
	}

	return r
}
