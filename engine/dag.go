package engine

import (
	"github.com/heimdalr/dag"
	"heph/utils/maps"
	"heph/utils/sets"
	"sort"
)

type DAG struct {
	*dag.DAG
}

// returns parents first
func (e *DAG) orderedWalker(target *Target, rel func(*Target) ([]*Target, error), ancsm map[string]struct{}, minDepth, depth int, f func(*Target)) error {
	if _, ok := ancsm[target.FQN]; ok {
		return nil
	}
	ancsm[target.FQN] = struct{}{}

	parents, err := rel(target)
	if err != nil {
		return err
	}

	for _, parent := range parents {
		err := e.orderedWalker(parent, rel, ancsm, minDepth, depth+1, f)
		if err != nil {
			return err
		}
	}

	if depth >= minDepth {
		f(target)
	}

	return nil
}

func (e *DAG) GetOrderedAncestors(targets []*Target, includeRoot bool) ([]*Target, error) {
	ancs := make([]*Target, 0)

	err := e.getOrderedAncestors(targets, includeRoot, func(target *Target) {
		ancs = append(ancs, target)
	})

	return ancs, err
}

func (e *DAG) GetOrderedAncestorsWithOutput(targets []*Target, includeRoot bool) ([]*Target, *maps.Map[string, *sets.Set[string, string]], error) {
	ancs := make([]*Target, 0)
	ancsout := &maps.Map[string, *sets.Set[string, string]]{
		Default: func() *sets.Set[string, string] {
			return sets.NewSet(func(s string) string {
				return s
			}, 0)
		},
	}

	addOut := func(t *Target, output string) {
		if output == "" && !t.OutWithSupport.HasName(output) {
			return
		}

		ancsout.Get(t.FQN).Add(output)
	}

	addAllOut := func(t *Target) {
		ancsout.Get(t.FQN).AddAll(t.OutWithSupport.Names())
	}

	maybeAddAllOuts := func(t *Target, output string) {
		if t.HasSupportFiles || len(t.Codegen) > 0 || Contains(targets, t.FQN) {
			addAllOut(t)
		} else {
			addOut(t, output)
		}
	}

	err := e.getOrderedAncestors(targets, includeRoot, func(target *Target) {
		deps := target.Deps.All().Merge(target.HashDeps)
		for _, dep := range deps.Targets {
			maybeAddAllOuts(dep.Target, dep.Output)
		}

		for _, tool := range target.Tools.Targets {
			maybeAddAllOuts(tool.Target, tool.Output)
		}

		ancs = append(ancs, target)
	})

	return ancs, ancsout, err
}

func (e *DAG) getOrderedAncestors(targets []*Target, includeRoot bool, f func(*Target)) error {
	ancsm := map[string]struct{}{}

	minDepth := 1
	if includeRoot {
		minDepth = 0
	}

	for _, target := range targets {
		err := e.orderedWalker(target, e.GetParents, ancsm, minDepth, 0, f)
		if err != nil {
			return err
		}
	}

	return nil
}

func (e *DAG) GetOrderedDescendants(targets []*Target, includeRoot bool) ([]*Target, error) {
	ancs := make([]*Target, 0)
	ancsm := map[string]struct{}{}

	minDepth := 1
	if includeRoot {
		minDepth = 0
	}

	for _, target := range targets {
		err := e.orderedWalker(target, e.GetChildren, ancsm, minDepth, 0, func(target *Target) {
			ancs = append(ancs, target)
		})
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
