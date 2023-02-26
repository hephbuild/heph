package engine

import (
	"github.com/heimdalr/dag"
	"heph/targetspec"
	"heph/tgt"
	"heph/utils/maps"
	"heph/utils/sets"
	"sort"
)

type DAG struct {
	*dag.DAG
}

// returns parents first
func (d *DAG) orderedWalker(target *tgt.Target, rel func(base targetspec.TargetBase) ([]*tgt.Target, error), ancsm map[string]struct{}, minDepth, depth int, f func(*tgt.Target)) error {
	if _, ok := ancsm[target.FQN]; ok {
		return nil
	}
	ancsm[target.FQN] = struct{}{}

	parents, err := rel(target)
	if err != nil {
		return err
	}

	for _, parent := range parents {
		err := d.orderedWalker(parent, rel, ancsm, minDepth, depth+1, f)
		if err != nil {
			return err
		}
	}

	if depth >= minDepth {
		f(target)
	}

	return nil
}

func (d *DAG) GetOrderedAncestors(targets []*tgt.Target, includeRoot bool) ([]*tgt.Target, error) {
	ancs := make([]*tgt.Target, 0)

	err := d.getOrderedAncestors(targets, includeRoot, func(target *tgt.Target) {
		ancs = append(ancs, target)
	})

	return ancs, err
}

func (d *DAG) GetOrderedAncestorsWithOutput(targets []*tgt.Target, includeRoot bool) ([]*tgt.Target, *maps.Map[string, *sets.Set[string, string]], error) {
	ancs := make([]*tgt.Target, 0)
	ancsout := &maps.Map[string, *sets.Set[string, string]]{
		Default: func(k string) *sets.Set[string, string] {
			return sets.NewSet(func(s string) string {
				return s
			}, 0)
		},
	}

	addOut := func(t *tgt.Target, output string) {
		if output == "" && !t.OutWithSupport.HasName(output) {
			return
		}

		ancsout.Get(t.FQN).Add(output)
	}

	addAllOut := func(t *tgt.Target) {
		ancsout.Get(t.FQN).AddAll(t.OutWithSupport.Names())
	}

	maybeAddAllOuts := func(t *tgt.Target, output string) {
		if t.RestoreCache || t.HasSupportFiles || len(t.Codegen) > 0 || Contains(targets, t.FQN) {
			addAllOut(t)
		} else {
			addOut(t, output)
		}
	}

	err := d.getOrderedAncestors(targets, includeRoot, func(target *tgt.Target) {
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

func (d *DAG) getOrderedAncestors(targets []*tgt.Target, includeRoot bool, f func(*tgt.Target)) error {
	ancsm := map[string]struct{}{}

	minDepth := 1
	if includeRoot {
		minDepth = 0
	}

	for _, target := range targets {
		err := d.orderedWalker(target, d.GetParents, ancsm, minDepth, 0, f)
		if err != nil {
			return err
		}
	}

	return nil
}

func (d *DAG) GetOrderedDescendants(targets []*tgt.Target, includeRoot bool) ([]*tgt.Target, error) {
	ancs := make([]*tgt.Target, 0)
	ancsm := map[string]struct{}{}

	minDepth := 1
	if includeRoot {
		minDepth = 0
	}

	for _, target := range targets {
		err := d.orderedWalker(target, d.GetChildren, ancsm, minDepth, 0, func(target *tgt.Target) {
			ancs = append(ancs, target)
		})
		if err != nil {
			return nil, err
		}
	}

	return ancs, nil
}

func (d *DAG) GetAncestors(target targetspec.TargetBase) ([]*tgt.Target, error) {
	return d.GetAncestorsOfFQN(target.GetFQN())
}

func (d *DAG) GetAncestorsOfFQN(fqn string) ([]*tgt.Target, error) {
	ancestors, err := d.DAG.GetAncestors(fqn)
	if err != nil {
		return nil, err
	}

	return d.mapToArray(ancestors), nil
}

func (d *DAG) GetDescendants(target targetspec.TargetBase) ([]*tgt.Target, error) {
	return d.GetDescendantsOfFQN(target.GetFQN())
}

func (d *DAG) GetDescendantsOfFQN(fqn string) ([]*tgt.Target, error) {
	ancestors, err := d.DAG.GetDescendants(fqn)
	if err != nil {
		return nil, err
	}

	return d.mapToArray(ancestors), nil
}

func (d *DAG) GetParents(target targetspec.TargetBase) ([]*tgt.Target, error) {
	ancestors, err := d.DAG.GetParents(target.GetFQN())
	if err != nil {
		return nil, err
	}

	return d.mapToArray(ancestors), nil
}

func (d *DAG) GetVertices() []*tgt.Target {
	vertices := d.DAG.GetVertices()

	return d.mapToArray(vertices)
}

func (d *DAG) GetChildren(target targetspec.TargetBase) ([]*tgt.Target, error) {
	ancestors, err := d.DAG.GetChildren(target.GetFQN())
	if err != nil {
		return nil, err
	}

	return d.mapToArray(ancestors), nil
}

func (d *DAG) GetLeaves() []*tgt.Target {
	leaves := d.DAG.GetLeaves()

	return d.mapToArray(leaves)
}

func (d *DAG) AddVertex(target *tgt.Target) error {
	return d.DAG.AddVertexByID(target.FQN, target)
}

func (d *DAG) mapToArray(m map[string]interface{}) []*tgt.Target {
	a := make([]*tgt.Target, 0)
	for _, anci := range m {
		anc := anci.(*tgt.Target)
		a = append(a, anc)
	}

	sort.Slice(a, func(i, j int) bool {
		return a[i].FQN < a[j].FQN
	})

	return a
}

type Walker func(target *tgt.Target)

func (w Walker) Visit(vertexer dag.Vertexer) {
	_, t := vertexer.Vertex()
	w(t.(*tgt.Target))
}
