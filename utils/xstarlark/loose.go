package xstarlark

import (
	"fmt"
	"go.starlark.net/starlark"
)

type Listable[T any] []T

func (c *Listable[T]) Unpack(v starlark.Value) error {
	if _, ok := v.(starlark.NoneType); ok {
		return nil
	}

	if v, ok := v.(*starlark.List); ok {
		values := make([]T, 0, v.Len())
		it := v.Iterate()
		defer it.Done()

		var e starlark.Value
		var i int
		for it.Next(&e) {
			var single T
			err := starlark.UnpackPositionalArgs(fmt.Sprint(i), starlark.Tuple{v}, nil, 0, &single)
			if err != nil {
				return err
			}
			values = append(values, e.(T))
			i++
		}

		*c = values
		return nil
	}

	var single T
	err := starlark.UnpackPositionalArgs("value", starlark.Tuple{v}, nil, 0, &single)
	if err != nil {
		return err
	}

	*c = Listable[T]{single}

	return nil
}
