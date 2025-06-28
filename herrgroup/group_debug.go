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

func (g *Group) track(f func() error) error {
	g.m.Lock()
	i := len(g.done)
	g.done = append(g.done, false)
	g.m.Unlock()

	defer func() {
		g.done[i] = true
	}()

	return f()
}

func (g *Group) Go(f func() error) {
	g.Group.Go(func() error {
		return g.track(f)
	})
}

func (g *Group) TryGo(f func() error) bool {
	return g.Group.TryGo(func() error {
		return g.track(f)
	})
}
