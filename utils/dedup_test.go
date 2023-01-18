package utils

import (
	"fmt"
	"github.com/stretchr/testify/assert"
	"strconv"
	"testing"
)

func TestDedup(t *testing.T) {
	tests := []struct {
		in       []int
		expected []int
		new      bool
	}{
		{[]int{1, 2, 3}, []int{1, 2, 3}, false},
		{[]int{1, 1, 2, 3}, []int{1, 2, 3}, true},
		{[]int{1, 2, 2, 3}, []int{1, 2, 3}, true},
		{[]int{1, 2, 3, 3}, []int{1, 2, 3}, true},
		{[]int{1, 1, 2, 2, 3, 3}, []int{1, 2, 3}, true},
	}
	for _, test := range tests {
		t.Run(fmt.Sprint(test.in), func(t *testing.T) {
			out := Dedup(test.in, func(i int) string {
				return strconv.FormatInt(int64(i), 10)
			})
			assert.EqualValues(t, test.expected, out)
			if test.new {
				assert.NotEqual(t, fmt.Sprintf("%p", out), fmt.Sprintf("%p", test.in))
			} else {
				assert.Equal(t, fmt.Sprintf("%p", out), fmt.Sprintf("%p", test.in))
			}
		})
	}
}
