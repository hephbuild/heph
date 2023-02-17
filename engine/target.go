package engine

import (
	"errors"
	"heph/tgt"
	"heph/utils/flock"
	"heph/utils/fs"
	"sync"
)

type ActualOutNamedPaths = tgt.NamedPaths[fs.Paths, fs.Path]

type Target struct {
	*tgt.Target
	WorkdirRoot        fs.Path
	SandboxRoot        fs.Path
	actualSupportFiles fs.Paths
	actualOutFiles     *ActualOutNamedPaths
	OutExpansionRoot   *fs.Path

	processed  bool
	linked     bool
	deeplinked bool
	linking    bool
	linkingCh  chan struct{}
	linkingErr error
	// Deps + HashDeps + TargetTools
	linkingDeps *Targets
	m           sync.Mutex

	artifacts *ArtifactOrchestrator

	runLock         flock.Locker
	postRunWarmLock flock.Locker
	cacheLocks      map[string]flock.Locker
}

func (t *Target) transitivelyWalk(m map[string]struct{}, f func(t *Target) error) error {
	targets := append([]*Target{t}, t.linkingDeps.Slice()...)

	for _, t := range targets {
		if _, ok := m[t.FQN]; ok {
			continue
		}
		m[t.FQN] = struct{}{}

		err := f(t)
		if err != nil {
			return err
		}

		err = t.transitivelyWalk(m, f)
		if err != nil {
			return err
		}
	}

	return nil
}

func (t *Target) TransitivelyWalk(f func(t *Target) error) error {
	m := map[string]struct{}{}

	err := t.transitivelyWalk(m, f)
	if err != nil && !errors.Is(err, tgt.ErrStopWalk) {
		return err
	}

	return nil
}

func (t *Target) resetLinking() {
	t.deeplinked = false

	spec := t.TargetSpec

	if t.linkingErr != nil || len(spec.Deps.Exprs) > 0 || len(spec.HashDeps.Exprs) > 0 || len(spec.Tools.Exprs) > 0 {
		depsCap := 0
		if t.linkingDeps != nil {
			depsCap = len(t.linkingDeps.Slice())
		}
		t.linkingDeps = NewTargets(depsCap)
		t.linked = false
		t.linkingErr = nil
	}
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
	for _, clabel := range labels {
		for _, tlabel := range t.Labels {
			if clabel == tlabel {
				return true
			}
		}
	}

	return false
}
