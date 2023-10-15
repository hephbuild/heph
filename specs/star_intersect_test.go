package specs

import (
	"fmt"
	"github.com/stretchr/testify/assert"
	"testing"
)

func TestStarIntersect(t *testing.T) {
	tests := []struct {
		a, b      string
		expected  bool
		expectedm bool
	}{
		{"*", "*", true, true},
		{"**", "**", true, true},
		{"*", "**", true, true},
		{"**", "*/*", true, true},
		{"**", "*/a/*", true, true},
		//{"a/**", "*/a/*", true, true},
		{"**", "a/b/d/c/*", true, true},
		{"a/**/c/d", "a/b/d/c/*", true, true},
		{"a/**/c", "a/b/d/c/*", true, true},
		{"a/*c*/b", "a/fce/*", true, true},
		{"a/*c/b", "a/dec/*", true, true},
		{"*x/y", "x/y", true, true},
		{"x*/y", "x/y", true, true},
		{"*x*/y", "x/y", true, true},
		{"*x*/y", "ax/y", true, true},
		{"*x*/y", "xb/y", true, true},
		{"*x*/y", "axb/y", true, true},
		{"*x*/y", "abxcd/y", true, true},
		{"x*z/y", "xz/y", true, true},
		{"x*z/y", "xcz/y", true, true},
		{"x*z/y", "xcz/*", true, true},
		{"x*z/y", "xcz/**", true, true},
		{"x*z/y/a/b", "xcz/**", true, true},
		{"*x*/y", "x/y", true, true},
		{"**/x", "/x", true, true},

		{"a/**/c/d/x", "a/b/d/c/*", false, false},
		{"*/x", "x/y", false, false},
		{"*/x", "x/y", false, false},
		{"*/x", "x", false, false},

		{"", "", true, true},
		{"abc", "abc", true, true},
		{"ab*", "abc", true, true},
		{"abc", "ab*", false, false},
		{"a/b/c", "a/b/c", true, true},
		{"a/b/*", "a/b/c", true, true},
		//{"a/b/c", "a/b/*", false, false},
		{"a/b/**", "a/b/c", true, true},
		{"a/b/**", "a/b/*", true, true},
		{"a/b/**", "a/b", true, true},
		//{"a/b", "a/b/**", false, false},
		{"a/b/**", "a/b/c/d", true, true},
		{"**/c/d", "a/b/c/d", true, true},
		{"**/c/d", "/c/d", true, true},
		{"**/c/d", "c/d", true, true},
		{"a/**/d", "a/d", true, true},
		{"a/**/d", "a/b/c/d", true, true},
		{"a/**/d", "a/b*/c/d", true, true},
		{"**", "", true, true},
		{"**", "*", true, true},
		{"**", "**", true, true},
	}
	for _, test := range tests {
		t.Run(fmt.Sprintf("%v %v", test.a, test.b), func(t *testing.T) {
			actual := starIntersect(test.a, test.b, 0, 0)

			assert.Equal(t, test.expected, actual)

			//actualm := starMatch(test.a, test.b, 0, 0)
			//
			//assert.Equal(t, test.expectedm, actualm)
		})
	}
}
