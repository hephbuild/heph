//go:build errgroupdebug

package herrgroup

import (
	"golang.org/x/sync/errgroup"
	"sync"
)

type Group struct {
	errgroup.Group
	m    sync.Mutex
	done []bool
}

func (g *Group) track() func() {
	g.m.Lock()
	i := len(g.done)
	g.done = append(g.done, false)
	g.m.Unlock()

	return func() {
		g.done[i] = true
	}
}

func (g *Group) Go(f func() error) {
	done := g.track()

	g.Group.Go(func() error {
		defer done()

		return f()
	})
}

func (g *Group) TryGo(f func() error) bool {
	done := g.track()

	return g.Group.TryGo(func() error {
		defer done()

		return f()
	})
}
