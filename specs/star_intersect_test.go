package specs

import (
	"fmt"
	"github.com/stretchr/testify/assert"
	"testing"
)

func TestStarIntersect(t *testing.T) {
	tests := []struct {
		a, b     string
		expected bool
	}{
		{"*", "*", true},
		{"**", "**", true},
		{"*", "**", true},
		{"**", "*/*", true},
		{"**", "*/a/*", true},
		//{"a/**", "*/a/*", true},
		{"**", "a/b/d/c/*", true},
		{"a/**/c/d", "a/b/d/c/*", true},
		{"a/**/c", "a/b/d/c/*", true},
		{"a/*c*/b", "a/fce/*", true},
		{"a/*c/b", "a/dec/*", true},
		{"*x/y", "x/y", true},
		{"x*/y", "x/y", true},
		{"*x*/y", "x/y", true},
		{"*x*/y", "ax/y", true},
		{"*x*/y", "xb/y", true},
		{"*x*/y", "axb/y", true},
		{"*x*/y", "abxcd/y", true},
		{"x*z/y", "xz/y", true},
		{"x*z/y", "xcz/y", true},
		{"x*z/y", "xcz/*", true},
		{"x*z/y", "xcz/**", true},
		{"x*z/y/a/b", "xcz/**", true},
		{"*x*/y", "x/y", true},
		{"**/x", "/x", true},

		{"a/**/c/d/x", "a/b/d/c/*", false},
		{"*/x", "x/y", false},
		{"*/x", "x/y", false},
		{"*/x", "x", false},

		{"", "", true},
		{"abc", "abc", true},
		{"ab*", "abc", true},
		{"abc", "ab*", true},
		{"a/b/c", "a/b/c", true},
		{"a/b/*", "a/b/c", true},
		//{"a/b/c", "a/b/*", false},
		{"a/b/**", "a/b/c", true},
		{"a/b/**", "a/b/*", true},
		{"a/b/**", "a/b", true},
		//{"a/b", "a/b/**", false},
		{"a/b/**", "a/b/c/d", true},
		{"**/c/d", "a/b/c/d", true},
		{"**/c/d", "/c/d", true},
		{"**/c/d", "c/d", true},
		{"a/**/d", "a/d", true},
		{"a/**/d", "a/b/c/d", true},
		{"a/**/d", "a/b*/c/d", true},
		{"**", "", true},
		{"**", "*", true},
		{"**", "**", true},
	}
	for _, test := range tests {
		t.Run(fmt.Sprintf("%v %v", test.a, test.b), func(t *testing.T) {
			actual := starIntersect(test.a, test.b, 0, 0)

			assert.Equal(t, test.expected, actual)
		})
	}
}
