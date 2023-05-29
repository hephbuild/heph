package graph

import (
	"fmt"
)

func NewTargetNotFoundError(target string) error {
	return TargetNotFoundErr{
		String: target,
	}
}

type TargetNotFoundErr struct {
	String string
}

func (e TargetNotFoundErr) Error() string {
	if e.String == "" {
		return "target not found"
	}
	return fmt.Sprintf("target %v not found", e.String)
}

func (e TargetNotFoundErr) Is(err error) bool {
	_, ok := err.(TargetNotFoundErr)
	return ok
}
