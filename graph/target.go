package graph

import (
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/xfs"
	"sync"
)

type OutNamedPaths = tgt.NamedPaths[xfs.RelPaths, xfs.RelPath]

type Target struct {
	specs.Target

	Tools          TargetTools
	Deps           TargetNamedDeps
	HashDeps       TargetDeps
	OutWithSupport *OutNamedPaths
	Out            *OutNamedPaths
	Env            map[string]string
	RuntimeEnv     map[string]TargetRuntimeEnv
	RuntimePassEnv []string
	Platforms      []specs.Platform

	// Collected transitive deps from deps/tools
	TransitiveDeps TargetTransitive

	// Own transitive config
	OwnTransitive TargetTransitive
	// Own transitive config plus their own transitive
	DeepOwnTransitive TargetTransitive

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

	spec := t.Spec()

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

type Targeter interface {
	specs.Specer
	GraphTarget() *Target
}

func (t *Target) GraphTarget() *Target {
	return t
}

type TargetRuntimeEnv struct {
	Value  string
	Target *Target
}

func (t *Target) ToolTarget() TargetTool {
	if !t.IsTool() {
		panic("not a tool target")
	}

	ts := t.Spec().Tools.Targets[0]

	for _, tool := range t.Tools.Targets {
		if tool.Target.FQN == ts.Target {
			return tool
		}
	}

	panic("unable to find tool target bin")
}
