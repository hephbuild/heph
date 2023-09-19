package exprs

import (
	"fmt"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"regexp"
	"strings"
	"testing"
)

type A struct {
	Annotations map[string]interface{}
}

type B struct {
	A
}

type C struct {
	S []string
}

type D struct {
	A *A
}

func TestExecDeep(t *testing.T) {
	tests := []struct {
		obj      any
		expected string
	}{
		{
			A{
				Annotations: map[string]interface{}{
					"blah": "$(hello)",
				},
			},
			`exprs.A{Annotations:map[string]interface {}{"blah":"hello world"}}`,
		},
		{
			B{
				A{
					Annotations: map[string]interface{}{
						"blah": "$(hello)",
					},
				},
			},
			`exprs.B{A:exprs.A{Annotations:map[string]interface {}{"blah":"hello world"}}}`,
		},
		{
			C{
				S: []string{"$(hello)"},
			},
			`exprs.C{S:[]string{"hello world"}}`,
		},
		{
			C{
				S: nil,
			},
			`exprs.C{S:[]string(nil)}`,
		},
		{
			D{
				A: &A{
					Annotations: map[string]interface{}{
						"blah": "$(hello)",
					},
				},
			},
			`exprs.D{A:(*exprs.A)(***)}`,
		},
		{
			D{
				A: nil,
			},
			`exprs.D{A:(*exprs.A)(nil)}`,
		},
	}
	for _, test := range tests {
		v := *(&test.obj) // Make an addressable copy

		t.Run(fmt.Sprintf("%#v", test.obj), func(t *testing.T) {
			err := ExecDeep(&v, map[string]Func{
				"hello": func(expr Expr) (string, error) {
					return "hello world", nil
				},
			})

			require.NoError(t, err)

			if strings.Contains(test.expected, "***") {
				parts := strings.Split(test.expected, "***")
				for i, part := range parts {
					parts[i] = regexp.QuoteMeta(part)
				}

				r := strings.Join(parts, ".*")
				r = "^" + r + "$"

				assert.Regexp(t, r, fmt.Sprintf("%#v", v))
			} else {
				assert.Equal(t, test.expected, fmt.Sprintf("%#v", v))
			}
		})
	}
}
