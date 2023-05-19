package graph

import (
	"github.com/heimdalr/dag"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/utils/maps"
	"github.com/hephbuild/heph/utils/sets"
	"sort"
)

type DAG struct {
	*dag.DAG
	targets *Targets
}

// returns parents first
func (d *DAG) orderedWalker(target *Target, rel func(targetspec.Specer) ([]*Target, error), ancsm map[string]struct{}, minDepth, depth int, f func(*Target)) error {
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

func (d *DAG) GetOrderedAncestors(targets []*Target, includeRoot bool) ([]*Target, error) {
	ancs := make([]*Target, 0)

	err := d.getOrderedAncestors(targets, includeRoot, func(target *Target) {
		ancs = append(ancs, target)
	})

	return ancs, err
}

func (d *DAG) GetOrderedAncestorsWithOutput(targets *Targets, includeRoot bool) (*Targets, *maps.Map[string, *sets.Set[string, string]], error) {
	ancs := NewTargets(0)
	ancsout := &maps.Map[string, *sets.Set[string, string]]{
		Default: func(k string) *sets.Set[string, string] {
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
		if t.RestoreCache || t.HasSupportFiles || len(t.Codegen) > 0 || Contains(targets.Slice(), t.FQN) {
			addAllOut(t)
		} else {
			addOut(t, output)
		}
	}

	err := d.getOrderedAncestors(targets.Slice(), includeRoot, func(target *Target) {
		deps := target.Deps.All().Merge(target.HashDeps)
		for _, dep := range deps.Targets {
			maybeAddAllOuts(d.targets.Find(dep.Target.FQN), dep.Output)
		}

		for _, tool := range target.Tools.Targets {
			maybeAddAllOuts(d.targets.Find(tool.Target.FQN), tool.Output)
		}

		ancs.Add(target)
	})

	return ancs, ancsout, err
}

func (d *DAG) getOrderedAncestors(targets []*Target, includeRoot bool, f func(*Target)) error {
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

func (d *DAG) GetOrderedDescendants(targets []*Target, includeRoot bool) ([]*Target, error) {
	ancs := make([]*Target, 0)
	ancsm := map[string]struct{}{}

	minDepth := 1
	if includeRoot {
		minDepth = 0
	}

	for _, target := range targets {
		err := d.orderedWalker(target, d.GetChildren, ancsm, minDepth, 0, func(target *Target) {
			ancs = append(ancs, target)
		})
		if err != nil {
			return nil, err
		}
	}

	return ancs, nil
}

func (d *DAG) GetAncestors(target targetspec.Specer) ([]*Target, error) {
	return d.GetAncestorsOfFQN(target.Spec().FQN)
}

func (d *DAG) GetAncestorsOfFQN(fqn string) ([]*Target, error) {
	ancestors, err := d.DAG.GetAncestors(fqn)
	if err != nil {
		return nil, err
	}

	return d.mapToArray(ancestors), nil
}

func (d *DAG) GetDescendants(target targetspec.Specer) ([]*Target, error) {
	return d.GetDescendantsOfFQN(target.Spec().FQN)
}

func (d *DAG) GetDescendantsOfFQN(fqn string) ([]*Target, error) {
	ancestors, err := d.DAG.GetDescendants(fqn)
	if err != nil {
		return nil, err
	}

	return d.mapToArray(ancestors), nil
}

func (d *DAG) GetParents(target targetspec.Specer) ([]*Target, error) {
	ancestors, err := d.DAG.GetParents(target.Spec().FQN)
	if err != nil {
		return nil, err
	}

	return d.mapToArray(ancestors), nil
}

func (d *DAG) GetVertices() []*Target {
	vertices := d.DAG.GetVertices()

	return d.mapToArray(vertices)
}

func (d *DAG) GetChildren(target targetspec.Specer) ([]*Target, error) {
	ancestors, err := d.DAG.GetChildren(target.Spec().FQN)
	if err != nil {
		return nil, err
	}

	return d.mapToArray(ancestors), nil
}

func (d *DAG) GetLeaves() []*Target {
	leaves := d.DAG.GetLeaves()

	return d.mapToArray(leaves)
}

func (d *DAG) GetFileDescendants(paths []string, targets []*Target) ([]*Target, error) {
	descendants := NewTargets(0)

	for _, path := range paths {
		for _, target := range targets {
			for _, file := range target.HashDeps.Files {
				if file.RelRoot() == path {
					descendants.Add(target)
					break
				}
			}
		}
	}

	descendants.Sort()

	return descendants.Slice(), nil
}

func (d *DAG) mapToArray(m map[string]interface{}) []*Target {
	a := make([]*Target, 0)
	for _, anci := range m {
		anc := anci.(*Target)
		a = append(a, anc)
	}

	sort.Slice(a, func(i, j int) bool {
		return a[i].FQN < a[j].FQN
	})

	return a
}

type Walker func(target *Target)

func (w Walker) Visit(vertexer dag.Vertexer) {
	_, t := vertexer.Vertex()
	w(t.(*Target))
}
