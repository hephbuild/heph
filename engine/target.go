package engine

import (
	"github.com/hephbuild/heph/engine/graph"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/flock"
	"github.com/hephbuild/heph/utils/fs"
)

type ActualOutNamedPaths = tgt.NamedPaths[fs.Paths, fs.Path]

type Target struct {
	*graph.Target

	WorkdirRoot        fs.Path
	SandboxRoot        fs.Path
	actualSupportFiles fs.Paths
	actualOutFiles     *ActualOutNamedPaths
	OutExpansionRoot   *fs.Path

	runLock         flock.Locker
	postRunWarmLock flock.Locker
	cacheLocks      map[string]flock.Locker
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

func (t *Target) ActualSupportFiles() fs.Paths {
	if t.actualSupportFiles == nil {
		panic("actualSupportFiles is nil for " + t.FQN)
	}

	return t.actualSupportFiles
}

func (t *Target) HasAnyLabel(labels []string) bool {
	return ads.ContainsAny(t.Labels, labels)
}
