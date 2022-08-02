package engine

import (
	"github.com/heimdalr/dag"
	"sort"
)

type DAG struct {
	*dag.DAG
}

func (e *DAG) GetAncestors(target *Target) (Targets, error) {
	ancestors, err := e.DAG.GetAncestors(target.FQN)
	if err != nil {
		return nil, err
	}

	return e.mapToArray(ancestors), nil
}

func (e *DAG) GetParents(target *Target) (Targets, error) {
	ancestors, err := e.DAG.GetParents(target.FQN)
	if err != nil {
		return nil, err
	}

	return e.mapToArray(ancestors), nil
}

func (e *DAG) GetVertices() Targets {
	vertices := e.DAG.GetVertices()

	return e.mapToArray(vertices)
}

func (e *DAG) GetChildren(target *Target) (Targets, error) {
	ancestors, err := e.DAG.GetChildren(target.FQN)
	if err != nil {
		return nil, err
	}

	return e.mapToArray(ancestors), nil
}

func (e *DAG) GetLeaves() Targets {
	leaves := e.DAG.GetLeaves()

	return e.mapToArray(leaves)
}

func (e *DAG) mapToArray(m map[string]interface{}) Targets {
	a := make(Targets, 0)
	for _, anci := range m {
		anc := anci.(*Target)
		a = append(a, anc)
	}

	sort.SliceStable(a, func(i, j int) bool {
		return a[i].FQN < a[j].FQN
	})

	return a
}

type Walker func(target *Target)

func (w Walker) Visit(vertexer dag.Vertexer) {
	_, t := vertexer.Vertex()
	w(t.(*Target))
}
