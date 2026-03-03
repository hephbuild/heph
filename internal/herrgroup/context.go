package herrgroup

import (
	"context"
	"sync"
)

type ContextGroup struct {
	*Group
	ctx        context.Context
	ff         bool
	failedOnce sync.Once
}

func (g *ContextGroup) goCtxErr() {
	// make sure there is some error to be returned by .Wait
	g.Group.Go(func() error {
		return g.ctx.Err()
	})
}

func (g *ContextGroup) Go(f func(ctx context.Context) error) {
	if g.Group == nil {
		panic("you forgot to call NewContext")
	}

	if g.Failed() {
		g.failedOnce.Do(g.goCtxErr)

		return
	}

	g.Group.Go(func() error {
		if g.Failed() {
			return g.ctx.Err()
		}

		return f(g.ctx)
	})
}

func (g *ContextGroup) Failed() bool {
	return g.ff && g.ctx.Err() != nil
}

func NewContext(ctx context.Context, failFast bool) ContextGroup {
	var g Group
	if failFast {
		g, ctx = WithContext(ctx)
	}

	return ContextGroup{Group: &g, ctx: ctx, ff: failFast}
}
