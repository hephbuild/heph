package specs

import (
	"fmt"
)

type specer interface {
	Specs() Targets
}

func NewTargetNotFoundError(target string, targets specer) error {
	return TargetNotFoundErr{
		String:  target,
		Targets: targets,
	}
}

type TargetNotFoundErr struct {
	String  string
	Targets specer
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
