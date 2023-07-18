package lcache

import (
	"github.com/hephbuild/heph/tgt"
)

type TargetMetas = tgt.TargetMetas[*Target]

func NewTargetMetas(factory func(fqn string) *Target) *TargetMetas {
	return tgt.NewTargetMetas(factory)
}
