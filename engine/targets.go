package engine

import (
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/utils/sets"
	"sort"
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
}

func NewTargets(cap int) *Targets {
	s := sets.NewSet(func(t *Target) string {
		if t == nil {
			panic("target must not be nil")
		}

		return t.FQN
	}, cap)

	return &Targets{
		Set: s,
	}
}

func (ts *Targets) FQNs() []string {
	if ts == nil {
		return nil
	}

	fqns := make([]string, 0)
	for _, target := range ts.Slice() {
		fqns = append(fqns, target.FQN)
	}

	return fqns
}

func (ts *Targets) Specs() []targetspec.TargetSpec {
	if ts == nil {
		return nil
	}

	specs := make([]targetspec.TargetSpec, 0)
	for _, target := range ts.Slice() {
		specs = append(specs, target.TargetSpec)
	}

	return specs
}

func (ts *Targets) Public() *Targets {
	pts := NewTargets(ts.Len() / 2)

	for _, target := range ts.Slice() {
		if !target.IsPrivate() {
			pts.Add(target)
		}
	}

	return pts
}

func (ts *Targets) Sort() {
	if ts == nil {
		return
	}

	a := ts.Slice()

	sort.Slice(a, func(i, j int) bool {
		return a[i].FQN < a[j].FQN
	})
}

func (ts *Targets) Copy() *Targets {
	if ts == nil {
		return NewTargets(0)
	}

	return &Targets{
		Set: ts.Set.Copy(),
	}
}

func (ts *Targets) Find(fqn string) *Target {
	if ts == nil {
		return nil
	}

	return ts.GetKey(fqn)
}
