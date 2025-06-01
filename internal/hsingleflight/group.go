package hsingleflight

import (
	"fmt"
	"github.com/dlsniper/debugger"
	sync_map "github.com/zolstein/sync-map"
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
	c sync_map.Map[string, func() (T, error)]
	o sync.Once
}

func (g *GroupMem[T]) Set(key string, v T, err error) {
	g.c.LoadOrStore(key, sync.OnceValues(func() (T, error) {
		return v, err
	}))
}

func (g *GroupMem[T]) Do(key string, do func() (T, error)) (T, error, bool) {
	f, loaded := g.c.LoadOrStore(key, sync.OnceValues(do))
	v, err := f()

	return v, err, !loaded
}
