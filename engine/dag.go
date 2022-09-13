package engine

import (
	"github.com/heimdalr/dag"
	"sort"
)

type DAG struct {
	*dag.DAG
}

func (e *DAG) orderedWalker(target *Target, rel func(*Target) ([]*Target, error), ancs *[]*Target, ancsm map[string]struct{}, minDepth, depth int) error {
	if _, ok := ancsm[target.FQN]; ok {
		return nil
	}
	ancsm[target.FQN] = struct{}{}

	parents, err := rel(target)
	if err != nil {
		return err
	}

	for _, parent := range parents {
		err := e.orderedWalker(parent, rel, ancs, ancsm, minDepth, depth+1)
		if err != nil {
			return err
		}
	}

	if depth >= minDepth {
		*ancs = append(*ancs, target)
	}

	return nil
}

func (e *DAG) GetOrderedAncestors(targets []*Target, includeRoot bool) ([]*Target, error) {
	ancs := make([]*Target, 0)
	ancsm := map[string]struct{}{}

	minDepth := 1
	if includeRoot {
		minDepth = 0
	}

	for _, target := range targets {
		err := e.orderedWalker(target, e.GetParents, &ancs, ancsm, minDepth, 0)
		if err != nil {
			return nil, err
		}
	}

	return ancs, nil
}

func (e *DAG) GetOrderedDescendants(targets []*Target, includeRoot bool) ([]*Target, error) {
	ancs := make([]*Target, 0)
	ancsm := map[string]struct{}{}

	minDepth := 1
	if includeRoot {
		minDepth = 0
	}

	for _, target := range targets {
		err := e.orderedWalker(target, e.GetChildren, &ancs, ancsm, minDepth, 0)
		if err != nil {
			return nil, err
		}
	}

	return ancs, nil
}

func (e *DAG) GetAncestors(target *Target) ([]*Target, error) {
	return e.GetAncestorsOfFQN(target.FQN)
}

func (e *DAG) GetAncestorsOfFQN(fqn string) ([]*Target, error) {
	ancestors, err := e.DAG.GetAncestors(fqn)
	if err != nil {
		return nil, err
	}

	return e.mapToArray(ancestors), nil
}

func (e *DAG) GetDescendants(target *Target) ([]*Target, error) {
	return e.GetDescendantsOfFQN(target.FQN)
}

func (e *DAG) GetDescendantsOfFQN(fqn string) ([]*Target, error) {
	ancestors, err := e.DAG.GetDescendants(fqn)
	if err != nil {
		return nil, err
	}

	return e.mapToArray(ancestors), nil
}

func (e *DAG) GetParents(target *Target) ([]*Target, error) {
	ancestors, err := e.DAG.GetParents(target.FQN)
	if err != nil {
		return nil, err
	}

	return e.mapToArray(ancestors), nil
}

func (e *DAG) GetVertices() []*Target {
	vertices := e.DAG.GetVertices()

	return e.mapToArray(vertices)
}

func (e *DAG) GetChildren(target *Target) ([]*Target, error) {
	ancestors, err := e.DAG.GetChildren(target.FQN)
	if err != nil {
		return nil, err
	}

	return e.mapToArray(ancestors), nil
}

func (e *DAG) GetLeaves() []*Target {
	leaves := e.DAG.GetLeaves()

	return e.mapToArray(leaves)
}

func (e *DAG) mapToArray(m map[string]interface{}) []*Target {
	a := make([]*Target, 0)
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
