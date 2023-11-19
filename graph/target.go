package graph

import (
	"encoding/json"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/sets"
	"github.com/hephbuild/heph/utils/xfs"
	"sync"
)

type OutNamedPaths = tgt.NamedPaths[xfs.RelPaths, xfs.RelPath]

type Target struct {
	specs.Target

	GenSources        []*Target       `json:"-"`
	Tools             TargetTools     `json:"-"`
	Deps              TargetNamedDeps `json:"-"`
	RuntimeDeps       TargetNamedDeps `json:"-"`
	HashDeps          TargetDeps      `json:"-"`
	OutWithSupport    *OutNamedPaths
	Out               *OutNamedPaths
	RestoreCachePaths xfs.RelPaths
	Env               map[string]string
	RuntimeEnv        map[string]TargetRuntimeEnv
	RuntimePassEnv    []string
	Platforms         []specs.Platform

	// Collected transitive deps from deps/tools
	TransitiveDeps TargetTransitive

	// Own transitive config
	OwnTransitive TargetTransitive
	// Own transitive config plus their own transitive
	DeepOwnTransitive TargetTransitive

	Artifacts *ArtifactRegistry `json:"-"`

	processed  bool
	linked     bool
	deeplinked bool
	linking    bool
	linkingCh  chan struct{}
	linkingErr error
	// Deps + HashDeps + TargetTools
	AllTargetDeps *Targets `json:"-"`
	m             sync.Mutex
}

func (t *Target) ResetLinking() {
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
	return t.Addr
}

func (t *Target) String() string {
	return t.Addr
}

func (t *Target) HasAnyLabel(labels []string) bool {
	return ads.ContainsAny(t.Labels, labels)
}

func (t *Target) EmptyDeps() bool {
	return t.Tools.Empty() &&
		t.Deps.Empty() &&
		t.HashDeps.Empty() &&
		t.RuntimeDeps.Empty() &&
		len(t.Env) == 0 &&
		len(t.PassEnv) == 0 &&
		len(t.RuntimeEnv) == 0 &&
		len(t.RuntimePassEnv) == 0
}

func (t *Target) DeepGenSources() []*Target {
	srcs := sets.NewSet(func(t *Target) string {
		return t.Addr
	}, 0)
	queue := sets.NewSet(func(t *Target) string {
		return t.Addr
	}, 0)

	current := t
	for current.GenSources != nil {
		srcs.AddAll(current.GenSources)
		queue.AddAll(current.GenSources)

		if queue.Len() > 0 {
			current = queue.Pop(0)
		} else {
			current = nil
		}
	}

	return srcs.Slice()
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
	Target *Target `json:"-"`
}

func (v TargetRuntimeEnv) MarshalJSON() ([]byte, error) {
	m := struct {
		Value  string
		Target string `json:"omitempty"`
	}{
		Value: v.Value,
	}
	if v.Target != nil {
		m.Target = v.Target.Addr
	}

	return json.Marshal(m)
}

func (t *Target) ToolTarget() TargetTool {
	if !t.IsTool() {
		panic("not a tool target")
	}

	ts := t.Spec().Tools.Targets[0]

	for _, tool := range t.Tools.Targets {
		if tool.Target.Addr == ts.Target {
			return tool
		}
	}

	panic("unable to find tool target bin")
}
