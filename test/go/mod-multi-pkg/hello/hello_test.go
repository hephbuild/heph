package hello

import (
	"github.com/stretchr/testify/assert"
	"testing"
)

func TestSanity(t *testing.T) {
	assert.Equal(t, "hello", Hello())
}
