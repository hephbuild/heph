//go:build cooltag

package main

import (
	"github.com/stretchr/testify/assert"
	"runtime"
	"testing"
)

func TestSanity(t *testing.T) {
	var expected string
	switch runtime.GOOS {
	case "darwin":
		expected = "darwin tag!"
	case "linux":
		expected = "linux tag!"
	default:
		t.Fatal("unexpected runtime " + runtime.GOOS)
	}
	out := StringOS() + " " + StringTag()
	t.Log(out)
	assert.Equal(t, expected, out)
}
