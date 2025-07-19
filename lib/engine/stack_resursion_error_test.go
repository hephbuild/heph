package engine

import (
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"testing"
)

func TestStackRecursionError(t *testing.T) {
	serr1 := NewStackRecursionConnectError("foobar")

	assert.Equal(t, "aborted: stack recursion: foobar", serr1.Error())

	serr2, ok := AsStackRecursionError(serr1)
	require.True(t, ok)

	assert.Equal(t, "foobar", serr2.Stack)
}
