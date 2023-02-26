package engine

import (
	"heph/tgt"
	"heph/utils"
)

func Contains(ts []*tgt.Target, fqn string) bool {
	for _, t := range ts {
		if t.FQN == fqn {
			return true
		}
	}

	return false
}

func Downcast(ts []*Target) []*tgt.Target {
	return utils.Map(ts, func(t *Target) *tgt.Target {
		return t.Target
	})
}

type Targets struct {
	*tgt.Store[*Target]
}

func (ts *Targets) BaseTargets() *tgt.TargetsSet {
	s := tgt.NewTargetsSet(ts.Len())
	for _, target := range ts.Slice() {
		s.Add(target.Target)
	}
	return s
}

func NewTargets(cap int) *Targets {
	return &Targets{
		Store: tgt.NewSet[*Target](cap),
	}
}
