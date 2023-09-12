package lcache

import (
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/locks"
	"github.com/hephbuild/heph/utils/maps"
	"github.com/hephbuild/heph/utils/xfs"
	"sync"
	"time"
)

type ActualOutNamedPaths = tgt.NamedPaths[xfs.RelPaths, xfs.RelPath]

type Target struct {
	*graph.Target
	depsHash  string
	inputHash string

	outExpansionRoot xfs.Path
	expandLock       locks.Locker

	actualOutFiles          *ActualOutNamedPaths
	actualSupportFiles      xfs.RelPaths
	actualRestoreCacheFiles xfs.RelPaths

	multiCacheLocks            locks.Locker
	cacheLocks                 map[string]locks.Locker
	cacheHashInputTargetMutex  sync.Mutex
	cacheHashOutputTargetMutex maps.KMutex
	cacheHashOutput            *maps.Map[string, string]
	cacheHashInputPathsModtime map[string]time.Time
}

func (t *Target) String() string {
	return t.Addr
}

func (t *Target) OutExpansionRoot() xfs.Path {
	if t.outExpansionRoot == xfs.NilPath {
		panic("outExpansionRoot is nil for " + t.Addr)
	}

	return t.outExpansionRoot
}

func (t *Target) HasActualOutFiles() bool {
	return t.actualOutFiles != nil
}

func (t *Target) ActualOutFiles() *ActualOutNamedPaths {
	if t.actualOutFiles == nil {
		panic("actualOutFiles is nil for " + t.Addr)
	}

	return t.actualOutFiles
}

func (t *Target) ActualSupportFiles() xfs.RelPaths {
	if t.actualSupportFiles == nil {
		panic("actualSupportFiles is nil for " + t.Addr)
	}

	return t.actualSupportFiles
}

func (t *Target) ActualRestoreCacheFiles() xfs.RelPaths {
	if t.actualRestoreCacheFiles == nil {
		panic("actualRestoreCacheFiles is nil for " + t.Addr)
	}

	return t.actualRestoreCacheFiles
}
