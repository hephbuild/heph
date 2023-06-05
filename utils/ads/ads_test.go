package ads

import (
	"fmt"
	"github.com/stretchr/testify/assert"
	"testing"
)

func identical(s1, s2 []int) bool {
	if len(s1) != len(s2) {
		return false
	}

	return len(s1) == 0 || &s1[0] == &s2[0]
}

func TestFilter(t *testing.T) {
	tests := []struct {
		input    []int
		f        func(int) bool
		expected []int
		noalloc  bool
	}{
		{
			input: []int{1, 2, 3},
			f: func(i int) bool {
				return true
			},
			expected: []int{1, 2, 3},
			noalloc:  true,
		},
		{
			input: []int{1, 2, 3},
			f: func(i int) bool {
				return i != 1
			},
			expected: []int{2, 3},
		},
		{
			input: []int{1, 2, 3},
			f: func(i int) bool {
				return i != 2
			},
			expected: []int{1, 3},
		},
		{
			input: []int{1, 2, 3},
			f: func(i int) bool {
				return i != 3
			},
			expected: []int{1, 2},
		},
		{
			input: []int{1, 2, 3},
			f: func(i int) bool {
				return i%2 == 0
			},
			expected: []int{2},
		},
		{
			input: []int{},
			f: func(i int) bool {
				return i%2 == 0
			},
			expected: []int{},
			noalloc:  true,
		},
	}
	for _, test := range tests {
		t.Run(fmt.Sprint(test.expected), func(t *testing.T) {
			actual := Filter(test.input, test.f)
			assert.Equal(t, test.expected, actual)

			if test.noalloc {
				assert.True(t, identical(actual, test.input))
			}
		})
	}
}
