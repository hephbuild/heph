package utils

import (
	"fmt"
	"strings"
)

// $(collect "." include="go_src")

type ExprArg struct {
	Name  string
	Value string
}

type Expr struct {
	Function  string
	PosArgs   []string
	NamedArgs []ExprArg
}

func ExprParse(s string) (*Expr, error) {
	if !strings.HasPrefix(s, "$(") {
		return nil, fmt.Errorf("unhandled expr")
	}

	s = strings.TrimPrefix(s, "$(")
	s = strings.TrimSuffix(s, ")")
	s = strings.TrimSpace(s)

	parts := strings.Fields(s)

	if len(parts) < 1 {
		return nil, fmt.Errorf("missing function name")
	}

	e := &Expr{
		Function: parts[0],
	}

	for _, part := range parts[1:] {
		if strings.HasPrefix(part, `"`) {
			if e.NamedArgs != nil {
				return nil, fmt.Errorf("pos args can only be defined before named args")
			}
			e.PosArgs = append(e.PosArgs, strings.Trim(part, `"`))
		} else {
			parts := strings.SplitN(part, "=", 2)

			if len(parts) != 2 {
				return nil, fmt.Errorf("expected k=v, got `%v`", part)
			}

			e.NamedArgs = append(e.NamedArgs, ExprArg{
				Name:  parts[0],
				Value: strings.Trim(parts[1], `"'`),
			})
		}
	}

	return e, nil
}
