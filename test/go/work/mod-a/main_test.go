package main

import (
	"github.com/stretchr/testify/assert"
	"mod-b"
	"testing"
)

func TestHello(t *testing.T) {
	assert.Equal(t, "hello", mod_b.Hello())
}
