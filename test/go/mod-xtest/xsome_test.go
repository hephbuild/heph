package main_test

import (
	"github.com/stretchr/testify/assert"
	"testing"
)

func TestXSanity(t *testing.T) {
	t.Log("XHello world")

	// :)
	assert.Equal(t, 1, 1)
}
