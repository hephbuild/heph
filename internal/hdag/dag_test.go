package hdag

import (
	"fmt"
	"testing"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestBFSWalk(t *testing.T) {
	d := New[int](func(i int) string {
		return fmt.Sprintf("node-%v", i)
	})

	require.NoError(t, d.AddVertex(1))
	require.NoError(t, d.AddVertex(2))
	require.NoError(t, d.AddVertex(3))

	require.NoError(t, d.AddEdge(1, 2))
	require.NoError(t, d.AddEdge(1, 3))

	oned, err := d.GetGraph(1)
	require.NoError(t, err)

	collected := []int{}
	err = oned.BFSWalk(func(i int) error {
		collected = append(collected, i)
		return nil
	})
	require.NoError(t, err)

	assert.Equal(t, []int{1, 2, 3}, collected)
}

func TestGetVertices(t *testing.T) {
	d := New[int](func(i int) string {
		return fmt.Sprintf("node-%v", i)
	})

	require.NoError(t, d.AddVertex(1))
	require.NoError(t, d.AddVertex(2))
	require.NoError(t, d.AddVertex(3))

	require.NoError(t, d.AddEdge(1, 2))
	require.NoError(t, d.AddEdge(1, 3))

	vertices := d.GetVertices()

	collected := []int{}
	for _, v := range vertices {
		collected = append(collected, v)
	}
	assert.ElementsMatch(t, []int{1, 2, 3}, collected)
}
