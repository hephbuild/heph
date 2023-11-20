package graph

import (
	"github.com/heimdalr/dag"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/maps"
	"github.com/hephbuild/heph/utils/sets"
	"golang.org/x/exp/slices"
	"strings"
)

type DAG struct {
	*dag.DAG
}

// returns parents first
func (d *DAG) orderedWalker(target *Target, rel func(specs.Specer) ([]*Target, error), ancsm map[string]struct{}, minDepth, depth int, f func(*Target)) error {
	if _, ok := ancsm[target.Addr]; ok {
		return nil
	}
	ancsm[target.Addr] = struct{}{}

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
			return sets.NewStringSet(0)
		},
	}

	addOut := func(t *Target, output string) {
		if output == "" && !t.OutWithSupport.HasName(output) {
			return
		}

		ancsout.Get(t.Addr).Add(output)
	}

	addAllOut := func(t *Target) {
		ancsout.Get(t.Addr).AddAll(t.OutWithSupport.Names())
	}

	maybeAddAllOuts := func(t *Target, output string) {
		if t.RestoreCache.Enabled || t.HasSupportFiles || t.Codegen != specs.CodegenNone || targets.Has(t) {
			addAllOut(t)
		} else {
			addOut(t, output)
		}
	}

	err := d.getOrderedAncestors(targets.Slice(), includeRoot, func(target *Target) {
		deps := target.Deps.All().Merge(target.HashDeps).Merge(target.RuntimeDeps.All())
		for _, dep := range deps.Targets {
			maybeAddAllOuts(dep.Target, dep.Output)
		}

		for _, tool := range target.Tools.Targets {
			maybeAddAllOuts(tool.Target, tool.Output)
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

func (d *DAG) GetAncestors(target specs.Specer) ([]*Target, error) {
	return d.GetAncestorsOfAddr(target.Spec().Addr)
}

func (d *DAG) GetAncestorsOfAddr(addr string) ([]*Target, error) {
	ancestors, err := d.DAG.GetAncestors(addr)
	if err != nil {
		return nil, err
	}

	return d.mapToArray(ancestors), nil
}

func (d *DAG) GetDescendants(target specs.Specer) ([]*Target, error) {
	return d.GetDescendantsOfAddr(target.Spec().Addr)
}

func (d *DAG) GetDescendantsOfAddr(addr string) ([]*Target, error) {
	ancestors, err := d.DAG.GetDescendants(addr)
	if err != nil {
		return nil, err
	}

	return d.mapToArray(ancestors), nil
}

func (d *DAG) GetParents(target specs.Specer) ([]*Target, error) {
	ancestors, err := d.DAG.GetParents(target.Spec().Addr)
	if err != nil {
		return nil, err
	}

	return d.mapToArray(ancestors), nil
}

func (d *DAG) GetVertices() []*Target {
	vertices := d.DAG.GetVertices()

	return d.mapToArray(vertices)
}

func (d *DAG) GetChildren(target specs.Specer) ([]*Target, error) {
	ancestors, err := d.DAG.GetChildren(target.Spec().Addr)
	if err != nil {
		return nil, err
	}

	return d.mapToArray(ancestors), nil
}

func (d *DAG) GetLeaves() []*Target {
	leaves := d.DAG.GetLeaves()

	return d.mapToArray(leaves)
}

// GetFileChildren returns targets directly depending on file
func (d *DAG) GetFileChildren(paths []string, universe []*Target) []*Target {
	descendants := NewTargets(0)

	for _, path := range paths {
		for _, target := range universe {
			for _, file := range target.HashDeps.Files {
				if file.RelRoot() == path {
					descendants.Add(target)
					break
				}
			}
		}
	}

	descendants.Sort()

	return descendants.Slice()
}

func (d *DAG) mapToArray(m map[string]interface{}) []*Target {
	a := make([]*Target, 0, len(m))
	for _, anci := range m {
		anc := anci.(*Target)
		a = append(a, anc)
	}

	slices.SortFunc(a, func(a, b *Target) int {
		return strings.Compare(a.Addr, b.Addr)
	})

	return a
}

type Walker func(target *Target)

func (w Walker) Visit(vertexer dag.Vertexer) {
	_, t := vertexer.Vertex()
	w(t.(*Target))
}
