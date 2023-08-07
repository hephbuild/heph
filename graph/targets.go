package graph

import (
	"github.com/hephbuild/heph/cmd/heph/search"
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

func (ts *Targets) Suggest(s string) specs.Targets {
	return search.FuzzyFindTarget(ts.Specs(), s, 1)
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

func NewTargetsFrom(ts []*Target) *Targets {
	s := NewTargets(len(ts))
	s.AddAll(ts)

	return s
}

func (ts *Targets) Addrs() []string {
	if ts == nil {
		return nil
	}

	return ads.Map(ts.Slice(), func(t *Target) string {
		return t.Addr
	})
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

func (ts *Targets) Filter(m specs.Matcher) (*Targets, error) {
	switch m := m.(type) {
	case specs.TargetAddr:
		target := ts.Find(m.Full())
		if target == nil {
			return nil, specs.NewTargetNotFoundError(m.Full(), ts)
		}

		return NewTargetsFrom([]*Target{target}), nil
	case specs.TargetAddrs:
		if len(m) == 0 {
			return nil, nil
		}

		out := NewTargets(len(m))
		for _, tp := range m {
			target := ts.Find(tp.Full())
			if target == nil {
				return nil, specs.NewTargetNotFoundError(tp.Full(), ts)
			}

			out.Add(target)
		}

		return out, nil
	default:
		out := NewTargets(0)
		for _, target := range ts.Slice() {
			if m.Match(target) {
				out.Add(target)
			}
		}
		return out, nil
	}
}
