package hdag

import (
	"context"
	"fmt"
	"sync"
	"testing"

	"github.com/stretchr/testify/require"
)

type Vertex struct {
	ID string
}

func TestDAGAddVertex(t *testing.T) {
	d := New(func(v Vertex) string { return v.ID })

	err := d.AddVertex(Vertex{"A"})
	require.NoError(t, err)

	err = d.AddVertex(Vertex{"A"})
	require.Error(t, err)
	require.True(t, IsDuplicateVertexError(err), "expected duplicate vertex error, got %v", err)
}

func TestDAGAddEdgeCycle(t *testing.T) {
	d := New(func(v Vertex) string { return v.ID })

	d.AddVertex(Vertex{"A"})
	d.AddVertex(Vertex{"B"})
	d.AddVertex(Vertex{"C"})

	require.NoError(t, d.AddEdge(Vertex{"A"}, Vertex{"B"}))
	require.NoError(t, d.AddEdge(Vertex{"B"}, Vertex{"C"}))
	require.Error(t, d.AddEdge(Vertex{"C"}, Vertex{"A"}), "expected cycle error")
}

func TestDAGConcurrent(t *testing.T) {
	d := NewMeta[Vertex, int](func(v Vertex) string { return v.ID })

	var wg sync.WaitGroup
	for i := range 100 {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			v := Vertex{fmt.Sprintf("V%d", i)}
			_ = d.AddVertex(v)
		}(i)
	}
	wg.Wait() // Ensure all roots exist

	for i := 1; i < 100; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			v := Vertex{fmt.Sprintf("V%d", i)}
			parent := Vertex{fmt.Sprintf("V%d", i/2)}
			_ = d.AddEdgeMeta(parent, v, i*10)
		}(i)
	}
	wg.Wait()

	// Check metas
	for i := 1; i < 100; i++ {
		v := Vertex{fmt.Sprintf("V%d", i)}
		parent := Vertex{fmt.Sprintf("V%d", i/2)}
		metas := d.GetEdgeMeta(parent, v)
		require.Len(t, metas, 1)
		require.Equal(t, i*10, metas[0])
	}
}

func TestDAGBFS(t *testing.T) {
	d := New(func(v Vertex) string { return v.ID })

	d.AddVertex(Vertex{"A"})
	d.AddVertex(Vertex{"B"})
	d.AddVertex(Vertex{"C"})
	d.AddVertex(Vertex{"D"})

	d.AddEdge(Vertex{"A"}, Vertex{"B"})
	d.AddEdge(Vertex{"A"}, Vertex{"C"})
	d.AddEdge(Vertex{"B"}, Vertex{"D"})
	d.AddEdge(Vertex{"C"}, Vertex{"D"})

	ctx := context.Background()
	visited := make([]string, 0)
	for v := range d.BFSWalk(ctx) {
		visited = append(visited, v.ID)
	}

	require.NotEmpty(t, visited)
	require.Equal(t, "A", visited[0])
	require.Contains(t, visited, "B")
	require.Contains(t, visited, "C")
	require.Contains(t, visited, "D")
}

func TestSubgraphs(t *testing.T) {
	d := New(func(v Vertex) string { return v.ID })

	// A -> B -> C -> D
	d.AddVertex(Vertex{"A"})
	d.AddVertex(Vertex{"B"})
	d.AddVertex(Vertex{"C"})
	d.AddVertex(Vertex{"D"})
	d.AddEdge(Vertex{"A"}, Vertex{"B"})
	d.AddEdge(Vertex{"B"}, Vertex{"C"})
	d.AddEdge(Vertex{"C"}, Vertex{"D"})

	ctx := context.Background()
	anc, _ := d.GetAncestorsGraph(ctx, Vertex{"C"})

	var ancIDs []string
	for _, v := range anc.GetVertices() {
		ancIDs = append(ancIDs, v.ID)
	}
	require.Contains(t, ancIDs, "A", "incorrect ancestors")
	require.NotContains(t, ancIDs, "D", "incorrect ancestors")

	desc, _ := d.GetDescendantsGraph(ctx, Vertex{"B"})
	var descIDs []string
	for _, v := range desc.GetVertices() {
		descIDs = append(descIDs, v.ID)
	}
	require.NotContains(t, descIDs, "A", "incorrect descendants")
	require.Contains(t, descIDs, "C", "incorrect descendants")
	require.Contains(t, descIDs, "D", "incorrect descendants")

	graph, _ := d.GetGraph(ctx, Vertex{"C"})

	var total int
	for range graph.GetVertices() {
		total++
	}
	require.Equal(t, 4, total, "expected 4 vertices in total graph")
}
