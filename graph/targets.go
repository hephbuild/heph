package graph

import (
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/sets"
	"strings"
)

func Contains(ts []*Target, addr string) bool {
	for _, t := range ts {
		if t.Addr == addr {
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

		return t.Addr
	}, cap)

	return &Targets{
		Set: s,
	}
}

func (ts *Targets) Addrs() []string {
	if ts == nil {
		return nil
	}

	addrs := make([]string, 0, len(ts.Slice()))
	for _, target := range ts.Slice() {
		addrs = append(addrs, target.Addr)
	}

	return addrs
}

func (ts *Targets) Specs() specs.Targets {
	return ads.Map(ts.Slice(), func(t *Target) specs.Target {
		return t.Spec()
	})
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

	ads.Sort(a, func(i, j *Target) int {
		return strings.Compare(i.Addr, j.Addr)
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

func (ts *Targets) Find(addr string) *Target {
	if ts == nil {
		return nil
	}

	return ts.GetKey(addr)
}
