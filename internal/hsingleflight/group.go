package hsingleflight

import "golang.org/x/sync/singleflight"

type Group[T any] singleflight.Group

func (g *Group[T]) Do(key string, do func() (T, error)) (T, error, bool) {
	sf := (*singleflight.Group)(g)

	vi, err, shared := sf.Do(key, func() (interface{}, error) {
		v, err := do()
		return v, err
	})

	var v T
	if vv, ok := vi.(T); ok {
		v = vv
	}

	return v, err, shared
}
