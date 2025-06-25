package engine

import (
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"testing"
)

func TestStack(t *testing.T) {
	var err error
	s := Stack[string]{}

	s, err = s.Push("first")
	require.NoError(t, err)
	assert.Equal(t, "first", s.Print())

	s, err = s.Push("second")
	require.NoError(t, err)
	assert.Equal(t, "first -> second", s.Print())

	s, err = s.Push("third")
	require.NoError(t, err)
	assert.Equal(t, "first -> second -> third", s.Print())

	s, err = s.Push("second")
	require.Error(t, err)
	require.Equal(t, err.Error(), "stack recursion detected: first -> second -> third -> second")
}
