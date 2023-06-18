package engine

import (
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/utils/sets"
)

func Contains(ts []*Target, fqn string) bool {
	for _, t := range ts {
		if t.FQN == fqn {
			return true
		}
	}

	return false
}

type Targets struct {
	*sets.Set[string, *Target]
	graph *graph.Targets
}

func NewTargetsFromGraph(tsm *TargetMetas, gts *graph.Targets) *Targets {
	ts := NewTargets(gts.Len())
	for _, target := range gts.Slice() {
		ts.Add(tsm.Find(target))
	}
	return ts
}

func NewTargets(cap int) *Targets {
	return &Targets{
		Set: sets.NewSet(func(t *Target) string {
			if t == nil {
				panic("target must not be nil")
			}

			return t.FQN
		}, cap),
		graph: graph.NewTargets(cap),
	}
}

func (ts *Targets) Add(t *Target) bool {
	if ts.Set.Add(t) {
		return ts.graph.Add(t.Target)
	}
	return false
}

func (ts *Targets) Specs() targetspec.TargetSpecs {
	return ts.graph.Specs()
}
