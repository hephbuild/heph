package main

import (
	"embed"
	_ "embed"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"strings"
	"testing"
)

//go:embed file.txt
var file string

//go:embed *.txt
var files embed.FS

func TestContent(t *testing.T) {
	assert.Equal(t, "hello", strings.TrimSpace(file))
}

func TestList(t *testing.T) {
	entries, err := files.ReadDir(".")
	require.NoError(t, err)

	names := make([]string, 0)
	for _, entry := range entries {
		names = append(names, entry.Name())
	}

	assert.EqualValues(t, []string{"file.txt"}, names)
}
