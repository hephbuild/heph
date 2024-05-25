package graph

import (
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/worker2/dag"
)

type Node = *dag.Node[*Target]

var deepDoMapPool = dag.NewPool[Node]()

func TransitiveDependencies(targets []*Target) []*Target {
	var ancs []*Target
	DeepDoMany(targets, func(target *Target) {
		ancs = append(ancs, target)
	})
	return ancs
}

func DeepDoMany(targets []*Target, f func(*Target)) {
	targetNodes := ads.Map(targets, func(t *Target) Node {
		return t.Node
	})

	dag.DeepDoMany(
		dag.DeepDoOptions[*Target]{
			Func: func(n *dag.Node[*Target]) {
				f(n.V)
			},
			MemoizerFactory: func() (map[*dag.Node[*Target]]struct{}, func()) {
				m, clean := deepDoMapPool.Get()
				return m, clean
			},
		},
		targetNodes,
	)
}

// GetFileChildren returns targets directly depending on file
func GetFileChildren(paths []string, universe []*Target) []*Target {
	descendants := NewTargets(0)

	for _, path := range paths {
		target := getFileChildren(path, universe)
		if target != nil {
			descendants.Add(target)
		}
	}

	descendants.Sort()

	return descendants.Slice()
}

func getFileChildren(path string, universe []*Target) *Target {
	for _, target := range universe {
		for _, file := range target.Deps.All().Files {
			if file.Abs() == path {
				return target
			}
		}
		for _, file := range target.HashDeps.Files {
			if file.Abs() == path {
				return target

			}
		}
	}

	return nil
}
