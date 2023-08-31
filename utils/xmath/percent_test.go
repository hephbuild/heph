package xmath

import (
	"fmt"
	"github.com/stretchr/testify/assert"
	"testing"
)

func TestFormatPercent(t *testing.T) {
	tests := []struct {
		s        string
		percent  float64
		expected string
	}{
		{"Doing %P...", 0, "Doing 0%..."},
		{"Doing %P...", 10, "Doing 10%..."},
		{"Doing %P...", 100, "Doing 100%..."},
		{"Doing %P...", 200, "Doing..."},
	}
	for _, test := range tests {
		t.Run(fmt.Sprintf("%v %v", test.s, test.percent), func(t *testing.T) {
			actual := FormatPercent(test.s, test.percent)

			assert.Equal(t, test.expected, actual)
		})
	}
}
