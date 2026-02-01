package hslices

import (
	"strings"
	"testing"

	"github.com/stretchr/testify/require"
)

func TestPermute(t *testing.T) {
	tests := []struct {
		arr      string
		expected []string
	}{
		{"", []string{""}},
		{"a", []string{"a"}},
		{"ab", []string{"ab", "ba"}},
		{"abc", []string{"abc", "acb", "bac", "bca", "cba", "cab"}},
	}
	for _, test := range tests {
		t.Run(test.arr, func(t *testing.T) {
			arr := strings.Split(test.arr, "")
			var expected [][]string
			for _, e := range test.expected {
				expected = append(expected, strings.Split(e, ""))
			}

			res := [][]string{}
			for arr := range Permute(arr) {
				res = append(res, arr)
			}

			require.Equal(t, expected, res)
		})
	}
}
