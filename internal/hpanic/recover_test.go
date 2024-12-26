package hpanic

import (
	"fmt"
	"github.com/stretchr/testify/assert"
	"testing"
)

func TestRecoverPanic(t *testing.T) {
	err := Recover(func() error {
		panic("blah")
	})
	assert.ErrorContains(t, err, "blah")
}

func TestRecoverOutOfBound(t *testing.T) {
	err := Recover(func() error {
		a := []string{}

		_ = a[1]

		return nil
	})
	assert.ErrorContains(t, err, "runtime error: index out of range [1] with length 0")
}

func TestRecoverWrap(t *testing.T) {
	err := Recover(func() error {
		panic("blah")
	}, Wrap(func(err any) error {
		return fmt.Errorf("error: %v", err)
	}))
	assert.ErrorContains(t, err, "error: blah")
}
