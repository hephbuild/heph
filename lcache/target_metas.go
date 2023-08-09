package lcache

import (
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/hash"
	"github.com/hephbuild/heph/utils/maps"
)

type targetMetaKey struct {
	addr     string
	depshash string
}

type TargetMetas[T any] struct {
	m *maps.Map[targetMetaKey, T]
}

func (m *TargetMetas[T]) Find(gtarget graph.Targeter) T {
	target := gtarget.GraphTarget()

	idh := hash.NewHash()
	for _, addr := range target.AllTargetDeps.Addrs() {
		idh.String(addr)
	}

	return m.findAtHash(gtarget, idh.Sum())
}

func (m *TargetMetas[T]) findAtHash(spec specs.Specer, h string) T {
	return m.m.Get(targetMetaKey{
		addr:     spec.Spec().Addr,
		depshash: h,
	})
}

func (m *TargetMetas[T]) Delete(spec specs.Specer) {
	m.m.DeleteP(func(k targetMetaKey) bool {
		return k.addr == spec.Spec().Addr
	})
}

func NewTargetMetas[T any](factory func(k targetMetaKey) T) *TargetMetas[T] {
	return &TargetMetas[T]{
		m: &maps.Map[targetMetaKey, T]{Default: factory},
	}
}
