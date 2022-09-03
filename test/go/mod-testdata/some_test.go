package main

import (
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"os"
	"strings"
	"testing"
)

func TestSanity(t *testing.T) {
	b, err := os.ReadFile("./file.txt")
	require.NoError(t, err)

	assert.Equal(t, "hello", strings.TrimSpace(string(b)))
}
