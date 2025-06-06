package hsingleflight

import (
	sync_map "github.com/zolstein/sync-map"
	"sync/atomic"
)

type GroupMem[T any] struct {
	c  sync_map.Map[string, groupMemEntry[T]]
	o  Group[string, groupMemEntry[T]]
	cc atomic.Uint32
}

type groupMemEntry[T any] struct {
	v   T
	err error
	id  uint32
}

func (g *GroupMem[T]) Do(key string, do func() (T, error)) (T, error, bool) {
	res, ok := g.c.Load(key)
	if ok {
		return res.v, res.err, true
	}

	i := g.cc.Add(1)

	res, err, _ := g.o.Do(key, func() (groupMemEntry[T], error) {
		res, ok := g.c.Load(key)
		if ok {
			return res, nil
		}

		v, err := do()
		res = groupMemEntry[T]{v: v, err: err, id: i}

		g.c.Store(key, res)

		return res, nil
	})
	if err != nil {
		return res.v, err, false
	}

	executed := res.id == i

	return res.v, res.err, executed
}
