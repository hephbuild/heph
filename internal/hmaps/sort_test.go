package hmaps

import (
	"testing"

	"github.com/stretchr/testify/assert"
)

func TestSorted(t *testing.T) {
	tests := []struct {
		name     string
		input    map[string]string
		expected []string
	}{
		{
			name:     "empty map",
			input:    map[string]string{},
			expected: []string{},
		},
		{
			name: "single item",
			input: map[string]string{
				"a": "1",
			},
			expected: []string{"a"},
		},
		{
			name: "3 items",
			input: map[string]string{
				"b": "2",
				"a": "1",
				"c": "3",
			},
			expected: []string{"a", "b", "c"},
		},
		{
			name: "4 items",
			input: map[string]string{
				"b": "2",
				"f": "3",
				"a": "1",
				"c": "3",
				"e": "3",
				"d": "3",
			},
			expected: []string{"a", "b", "c", "d", "e", "f"},
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			result := []string{}
			for k := range Sorted(tt.input) {
				result = append(result, k)
			}

			assert.Equal(t, tt.expected, result)
		})
	}
}
