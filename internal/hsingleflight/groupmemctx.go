package hsingleflight

import (
	"context"
	"sync/atomic"

	sync_map "github.com/zolstein/sync-map"
)

type GroupMemContext[K comparable, T any] struct {
	c  sync_map.Map[K, groupMemContextEntry[T]]
	o  GroupCtx[K, groupMemContextEntry[T]]
	cc atomic.Uint32
}

type groupMemContextEntry[T any] struct {
	v   T
	err error
	id  uint32
}

func (g *GroupMemContext[K, T]) Do(ctx context.Context, key K, do func(ctx context.Context) (T, error)) (T, error, bool) {
	res, ok := g.c.Load(key)
	if ok {
		return res.v, res.err, false
	}

	i := g.cc.Add(1)

	res, _, _ = g.o.DoHandle(ctx, key, func(ctx context.Context) (groupMemContextEntry[T], error) {
		res, ok := g.c.Load(key)
		if ok {
			return res, res.err
		}

		v, err := do(ctx)

		return groupMemContextEntry[T]{v: v, err: err, id: i}, err
	}, func(ctx context.Context, res groupMemContextEntry[T], err error) {
		g.c.Store(key, res)
	})

	executed := res.id == i

	return res.v, res.err, executed
}
