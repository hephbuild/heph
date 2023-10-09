package atest

import (
	"github.com/stretchr/testify/assert"
	"testing"
)

func TestSanity(t *testing.T) {
	Hello()

	// :)
	assert.Equal(t, 1, 1)
}
