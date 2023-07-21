package xstarlark

import (
	"fmt"
	"go.starlark.net/starlark"
	"strings"
)

type unpackError struct {
	err error
}

func (e unpackError) Unwrap() error {
	return e.err
}

func (e unpackError) Error() string {
	s := e.err.Error()

	return strings.ReplaceAll(s, "XXX: for parameter 1: ", "")
}

func unpackSingle(v starlark.Value, target any) error {
	err := starlark.UnpackPositionalArgs("XXX", starlark.Tuple{v}, nil, 0, target)
	if err != nil {
		return unpackError{err}
	}
	return nil
}

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
			if _, ok := e.(starlark.NoneType); ok {
				continue
			}

			var single T
			err := unpackSingle(e, &single)
			if err != nil {
				return fmt.Errorf("index %v: %w", i, err)
			}
			values = append(values, single)
			i++
		}

		*c = values
		return nil
	}

	var single T
	err := unpackSingle(v, &single)
	if err != nil {
		return err
	}

	*c = Listable[T]{single}

	return nil
}
