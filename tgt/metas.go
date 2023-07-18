package tgt

import (
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/utils/maps"
)

type TargetMetas[T any] struct {
	m *maps.Map[string, T]
}

func (m *TargetMetas[T]) FindFQN(fqn string) T {
	return m.m.Get(fqn)
}

func (m *TargetMetas[T]) Find(spec targetspec.Specer) T {
	return m.FindFQN(spec.Spec().FQN)
}

func NewTargetMetas[T any](factory func(fqn string) T) *TargetMetas[T] {
	return &TargetMetas[T]{
		m: &maps.Map[string, T]{Default: factory},
	}
}
