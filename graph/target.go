package graph

import (
	"errors"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/ads"
	"sync"
)

type Target struct {
	*tgt.Target

	Artifacts *ArtifactOrchestrator

	processed  bool
	linked     bool
	deeplinked bool
	linking    bool
	linkingCh  chan struct{}
	linkingErr error
	// Deps + HashDeps + TargetTools
	LinkingDeps *Targets
	m           sync.Mutex
}

func (t *Target) transitivelyWalk(m map[string]struct{}, f func(t *Target) error) error {
	targets := append([]*Target{t}, t.LinkingDeps.Slice()...)

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
		if t.LinkingDeps != nil {
			depsCap = len(t.LinkingDeps.Slice())
		}
		t.LinkingDeps = NewTargets(depsCap)
		t.linked = false
		t.linkingErr = nil
	}
}

func (t *Target) ID() string {
	return t.FQN
}

func (t *Target) String() string {
	return t.FQN
}

func (t *Target) HasAnyLabel(labels []string) bool {
	return ads.ContainsAny(t.Labels, labels)
}

func (t *Target) EmptyDeps() bool {
	return t.Tools.Empty() &&
		t.Deps.Empty() &&
		len(t.Env) == 0 &&
		len(t.PassEnv) == 0 &&
		len(t.RuntimeEnv) == 0 &&
		len(t.RuntimePassEnv) == 0
}
