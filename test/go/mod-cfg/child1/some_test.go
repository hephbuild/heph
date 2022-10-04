package main

import (
	"github.com/stretchr/testify/assert"
	"os"
	"testing"
)

func TestEnv(t *testing.T) {
	assert.Equal(t, "hello1", os.Getenv("SOME_KEY"))
}
