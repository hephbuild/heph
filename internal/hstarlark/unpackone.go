package hstarlark

import (
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

func UnpackOne(v starlark.Value, target any) error {
	err := starlark.UnpackPositionalArgs("XXX", starlark.Tuple{v}, nil, 0, target)
	if err != nil {
		return unpackError{err}
	}
	return nil
}
