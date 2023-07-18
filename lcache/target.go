package lcache

import (
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/utils/locks"
)

type Target struct {
	*graph.Target

	cacheLocks map[string]locks.Locker
}
