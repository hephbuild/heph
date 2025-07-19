package engine

import (
	"testing"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestStack(t *testing.T) {
	var err error
	s := Stack[string]{}

	s, err = s.Push("first")
	require.NoError(t, err)
	assert.Equal(t, "first", s.StringWith(" -> "))

	s, err = s.Push("second")
	require.NoError(t, err)
	assert.Equal(t, "first -> second", s.StringWith(" -> "))

	s, err = s.Push("third")
	require.NoError(t, err)
	assert.Equal(t, "first -> second -> third", s.StringWith(" -> "))

	s, err = s.Push("second")
	require.Error(t, err)
	require.Equal(t, "stack recursion detected: first -> second -> third -> second", err.Error())

	_ = s
}
