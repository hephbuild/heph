package pluginsdkconnect

import (
	"testing"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestStackRecursionError(t *testing.T) {
	serr1 := NewStackRecursionError("foobar")

	assert.Equal(t, "aborted: stack recursion: foobar", serr1.Error())

	serr2, ok := AsStackRecursionError(serr1)
	require.True(t, ok)

	assert.Equal(t, "foobar", serr2.GetStack())
}
