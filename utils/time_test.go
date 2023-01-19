package utils

import (
	"fmt"
	"github.com/stretchr/testify/assert"
	"testing"
	"time"
)

func TestFormatDuration(t *testing.T) {
	tests := []struct {
		d time.Duration
		e string
	}{
		{time.Millisecond, "1ms"},
		{99 * time.Millisecond, "99ms"},
		{100 * time.Millisecond, "0.1s"},
		{500 * time.Millisecond, "0.5s"},
		{1500 * time.Millisecond, "1.5s"},
		{time.Second, "1.0s"},
		{3 * time.Second, "3.0s"},
		{3*time.Second + time.Microsecond, "3.0s"},
		{15000 * time.Millisecond, "15.0s"},
		{60 * time.Second, "1m0s"},
	}
	for _, test := range tests {
		t.Run(fmt.Sprint(test.d), func(t *testing.T) {
			actual := FormatDuration(test.d)
			assert.Equal(t, test.e, actual)
		})
	}
}
