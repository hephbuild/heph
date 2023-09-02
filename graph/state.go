package graph

import (
	"github.com/heimdalr/dag"
	"github.com/hephbuild/heph/config"
	"github.com/hephbuild/heph/hroot"
	"github.com/hephbuild/heph/utils/maps"
	"github.com/hephbuild/heph/utils/sets"
	"github.com/hephbuild/heph/utils/xfs"
)

type State struct {
	Root   *hroot.State
	Config *config.Config

	targetsLock  maps.KMutex
	targets      *Targets
	Tools        *Targets
	codegenPaths map[string]*Target
	dag          *DAG
	labels       *sets.StringSet
}

func (e *State) CodegenPaths() map[string]*Target {
	return e.codegenPaths
}

func (e *State) DAG() *DAG {
	return e.dag
}

func (e *State) Targets() *Targets {
	return e.targets
}

func (e *State) Labels() *sets.StringSet {
	return e.labels
}

func (e *State) registerLabels(labels []string) {
	for _, label := range labels {
		e.labels.Add(label)
	}
}

func (e *State) HasLabel(label string) bool {
	return e.labels.Has(label)
}

func (e *State) GeneratedTargets() []*Target {
	targets := make([]*Target, 0)

	for _, target := range e.targets.Slice() {
		if !target.Gen {
			continue
		}

		targets = append(targets, target)
	}

	return targets
}

func (e *State) GetCodegenOrigin(path string) (*Target, bool) {
	if dep, ok := e.codegenPaths[path]; ok {
		return dep, ok
	}

	for cpath, dep := range e.codegenPaths {
		if ok, _ := xfs.PathMatchExactOrPrefixAny(path, cpath); ok {
			return dep, true
		}
	}

	return nil, false
}

func (e *State) linkedTargets() *Targets {
	targets := NewTargets(e.targets.Len())

	for _, target := range e.targets.Slice() {
		if !target.linked {
			continue
		}

		targets.Add(target)
	}

	return targets
}

func (e *State) toolTargets() *Targets {
	targets := NewTargets(e.targets.Len())

	for _, target := range e.targets.Slice() {
		if !target.IsTool() {
			continue
		}

		targets.Add(target)
	}

	return targets
}

func NewState(root *hroot.State, cfg *config.Config) *State {
	return &State{
		Root:         root,
		Config:       cfg,
		targetsLock:  maps.KMutex{},
		targets:      NewTargets(0),
		Tools:        NewTargets(0),
		codegenPaths: map[string]*Target{},
		dag:          &DAG{dag.NewDAG()},
		labels:       sets.NewStringSet(0),
	}
}
