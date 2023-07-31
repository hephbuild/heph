package graph

import (
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/maps"
)

type TargetMetas[T any] struct {
	m *maps.Map[string, T]
}

func (m *TargetMetas[T]) FindAddr(addr string) T {
	return m.m.Get(addr)
}

func (m *TargetMetas[T]) Find(spec specs.Specer) T {
	return m.FindAddr(spec.Spec().Addr)
}

func NewTargetMetas[T any](factory func(addr string) T) *TargetMetas[T] {
	return &TargetMetas[T]{
		m: &maps.Map[string, T]{Default: factory},
	}
}
