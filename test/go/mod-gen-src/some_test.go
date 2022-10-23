package main

import (
	_ "embed"
	"github.com/stretchr/testify/assert"
	"testing"
)

//go:embed hello.txt
var embedFile string

func TestHello(t *testing.T) {
	t.Log(Hello())
	t.Log(embedFile)
	assert.Equal(t, "hello", Hello())
	assert.Equal(t, "hello", embedFile)
}
