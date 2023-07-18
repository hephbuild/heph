package engine

import (
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/locks"
	"github.com/hephbuild/heph/utils/xfs"
)

type ActualOutNamedPaths = tgt.NamedPaths[xfs.Paths, xfs.Path]

type Target struct {
	*graph.Target

	WorkdirRoot        xfs.Path
	SandboxRoot        xfs.Path
	actualSupportFiles xfs.Paths
	actualOutFiles     *ActualOutNamedPaths
	OutExpansionRoot   *xfs.Path

	runLock         locks.Locker
	postRunWarmLock locks.Locker
}

func (t *Target) ID() string {
	return t.FQN
}

func (t *Target) ActualOutFiles() *ActualOutNamedPaths {
	if t.actualOutFiles == nil {
		panic("actualOutFiles is nil for " + t.FQN)
	}

	return t.actualOutFiles
}

func (t *Target) String() string {
	return t.FQN
}

func (t *Target) ActualSupportFiles() xfs.Paths {
	if t.actualSupportFiles == nil {
		panic("actualSupportFiles is nil for " + t.FQN)
	}

	return t.actualSupportFiles
}
