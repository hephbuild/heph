package hstarlark

import (
	"fmt"

	"go.starlark.net/starlark"
)

type List[T any] []T

func (c *List[T]) Unpack(v starlark.Value) error {
	if _, ok := v.(starlark.NoneType); ok {
		return nil
	}

	if v, ok := v.(*starlark.List); ok {
		it := v.Iterate()
		defer it.Done()

		values := make([]T, 0, v.Len())
		var e starlark.Value
		i := -1
		for it.Next(&e) {
			i++
			if _, ok := e.(starlark.NoneType); ok {
				continue
			}

			var single T
			err := UnpackOne(e, &single)
			if err != nil {
				return fmt.Errorf("index %v: %w", i, err)
			}
			values = append(values, single)
		}

		*c = values
		return nil
	}

	var single T
	err := UnpackOne(v, &single)
	if err != nil {
		return err
	}

	*c = List[T]{single}

	return nil
}
