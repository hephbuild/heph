package graph

import (
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
	AllTargetDeps *Targets
	m             sync.Mutex
}

func (t *Target) resetLinking() {
	t.deeplinked = false

	spec := t.TargetSpec

	if t.linkingErr != nil || len(spec.Deps.Exprs) > 0 || len(spec.HashDeps.Exprs) > 0 || len(spec.Tools.Exprs) > 0 {
		depsCap := 0
		if t.AllTargetDeps != nil {
			depsCap = len(t.AllTargetDeps.Slice())
		}
		t.AllTargetDeps = NewTargets(depsCap)
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
		t.HashDeps.Empty() &&
		len(t.Env) == 0 &&
		len(t.PassEnv) == 0 &&
		len(t.RuntimeEnv) == 0 &&
		len(t.RuntimePassEnv) == 0
}
