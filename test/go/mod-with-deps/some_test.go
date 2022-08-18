package main

import (
	"github.com/stretchr/testify/assert"
	"testing"
)

func TestSanity(t *testing.T) {
	t.Log("Hello world")

	// :)
	assert.Equal(t, 1, 1)
}
