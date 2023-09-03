package specs

import (
	"fmt"
)

type specer interface {
	Suggest(s string) Targets
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

	if e.Targets != nil {
		suggestions := e.Targets.Suggest(e.String).Addrs()
		if len(suggestions) > 0 {
			return fmt.Sprintf("target %v not found (did you mean %v ?)", e.String, suggestions[0])
		}
	}

	return fmt.Sprintf("target %v not found", e.String)
}

func (e TargetNotFoundErr) Is(err error) bool {
	_, ok := err.(TargetNotFoundErr)
	return ok
}
