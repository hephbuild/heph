package engine

import (
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/maps"
)

type TargetMetas struct {
	m *maps.Map[string, *Target]
}

func (m *TargetMetas) FindFQN(fqn string) *Target {
	return m.m.Get(fqn)
}

func (m *TargetMetas) FindGraph(target *graph.Target) *Target {
	return m.FindFQN(target.FQN)
}

func (m *TargetMetas) FindTGT(target *tgt.Target) *Target {
	return m.FindFQN(target.FQN)
}

func NewTargetMetas(factory func(fqn string) *Target) *TargetMetas {
	return &TargetMetas{
		m: &maps.Map[string, *Target]{Default: func(fqn string) *Target {
			return factory(fqn)
		}},
	}
}
