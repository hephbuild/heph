package ads

import (
	"fmt"
	"github.com/stretchr/testify/assert"
	"testing"
)

func sameSlice(s1, s2 []int) bool {
	if len(s1) != len(s2) {
		return false
	}

	if s1 == nil || s2 == nil {
		return s1 == nil && s2 == nil
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
			input: []int{},
			f: func(i int) bool {
				return true
			},
			expected: []int{},
			noalloc:  true,
		},
		{
			input: nil,
			f: func(i int) bool {
				return true
			},
			expected: nil,
			noalloc:  true,
		},
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
				assert.True(t, sameSlice(actual, test.input))
			}
		})
	}
}

func TestMapFlat(t *testing.T) {
	tests := []struct {
		input    []int
		f        func(int) []int
		expected []int
		noalloc  bool
	}{
		{
			input: []int{},
			f: func(i int) []int {
				return []int{i}
			},
			expected: []int{},
			noalloc:  true,
		},
		{
			input: nil,
			f: func(i int) []int {
				return []int{i}
			},
			expected: nil,
			noalloc:  true,
		},
		{
			input: []int{1, 2, 3},
			f: func(i int) []int {
				return []int{i}
			},
			expected: []int{1, 2, 3},
			noalloc:  true,
		},
		{
			input: []int{1, 2, 3},
			f: func(i int) []int {
				return []int{i, i}
			},
			expected: []int{1, 1, 2, 2, 3, 3},
		},
		{
			input: []int{1, 2, 3},
			f: func(i int) []int {
				if i == 2 {
					return []int{i, i}
				}
				return []int{i}
			},
			expected: []int{1, 2, 2, 3},
		},
	}
	for _, test := range tests {
		t.Run(fmt.Sprint(test.expected), func(t *testing.T) {
			actual := MapFlat(test.input, test.f)
			assert.Equal(t, test.expected, actual)

			if test.noalloc {
				assert.True(t, sameSlice(actual, test.input))
			}
		})
	}
}
