package main

import (
	"github.com/stretchr/testify/assert"
	"mod-multi-pkg/hello"
	"testing"
)

func TestSanity(t *testing.T) {
	assert.Equal(t, "hello", hello.Hello())
}
