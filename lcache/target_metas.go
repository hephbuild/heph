package lcache

import (
	"github.com/hephbuild/heph/graph"
)

type TargetMetas = graph.TargetMetas[*Target]

func NewTargetMetas(factory func(addr string) *Target) *TargetMetas {
	return graph.NewTargetMetas(factory)
}
