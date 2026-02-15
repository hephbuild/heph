package herrgroup

import "context"

type ContextGroup struct {
	*Group
	ctx context.Context
	ff  bool
}

func (g *ContextGroup) Go(f func(ctx context.Context) error) {
	if g.Failed() && g.ff {
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
	return g.ctx.Err() != nil
}

func NewContext(ctx context.Context, failFast bool) ContextGroup {
	var g Group
	if failFast {
		g, ctx = WithContext(ctx)
	}

	return ContextGroup{Group: &g, ctx: ctx, ff: failFast}
}
