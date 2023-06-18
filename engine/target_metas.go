package engine

import (
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/utils/maps"
)

type TargetMetas struct {
	m *maps.Map[string, *Target]
}

func (m *TargetMetas) FindFQN(fqn string) *Target {
	return m.m.Get(fqn)
}

func (m *TargetMetas) Find(spec targetspec.Specer) *Target {
	return m.FindFQN(spec.Spec().FQN)
}

func NewTargetMetas(factory func(fqn string) *Target) *TargetMetas {
	return &TargetMetas{
		m: &maps.Map[string, *Target]{Default: factory},
	}
}
