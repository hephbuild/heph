//go:build errgroupdebug

package herrgroup

import (
	"context"
	"sync"

	"golang.org/x/sync/errgroup"
)

type Group struct {
	g    *errgroup.Group
	m    sync.Mutex
	done []bool
}

func (g *Group) track() func() {
	g.m.Lock()
	if g.g == nil {
		g.g = &errgroup.Group{}
	}

	i := len(g.done)
	g.done = append(g.done, false)
	g.m.Unlock()

	return func() {
		g.done[i] = true
	}
}

func (g *Group) Go(f func() error) {
	done := g.track()

	g.g.Go(func() error {
		defer done()

		return f()
	})
}

func (g *Group) TryGo(f func() error) bool {
	done := g.track()

	return g.g.TryGo(func() error {
		defer done()

		return f()
	})
}

func (g *Group) Wait() error {
	if g.g == nil {
		return nil
	}

	return g.g.Wait()
}

func WithContext(ctx context.Context) (Group, context.Context) {
	g, ctx := errgroup.WithContext(ctx)

	return Group{g: g}, ctx //nolint:govet
}
