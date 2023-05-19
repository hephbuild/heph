package graph

import (
	"fmt"
	"github.com/c2fo/vfs/v6"
	"github.com/heimdalr/dag"
	"github.com/hephbuild/heph/config"
	"github.com/hephbuild/heph/engine/hroot"
	"github.com/hephbuild/heph/utils"
	"github.com/hephbuild/heph/utils/maps"
	"github.com/hephbuild/heph/utils/sets"
	"github.com/hephbuild/heph/vfssimple"
	"strings"
)

type State struct {
	Root   *hroot.State
	Config *Config

	targetsLock  maps.KMutex
	targets      *Targets
	tools        *Targets
	codegenPaths map[string]*Target
	dag          *DAG
	labels       *sets.StringSet
}

type Config struct {
	*config.Config
	Caches []CacheConfig
}

type CacheConfig struct {
	Name string
	config.Cache
	Location vfs.Location `yaml:"-"`
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
		if ok, _ := utils.PathMatchExactOrPrefixAny(path, cpath); ok {
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

func NewState(root *hroot.State, cfg *config.Config) (*State, error) {
	targets := NewTargets(0)
	s := &State{
		Root:         root,
		Config:       &Config{Config: cfg},
		targetsLock:  maps.KMutex{},
		targets:      targets,
		tools:        NewTargets(0),
		codegenPaths: map[string]*Target{},
		dag:          &DAG{dag.NewDAG(), targets},
		labels:       sets.NewStringSet(0),
	}

	for name, cache := range cfg.Caches {
		uri := cache.URI
		if !strings.HasSuffix(uri, "/") {
			uri += "/"
		}
		loc, err := vfssimple.NewLocation(uri)
		if err != nil {
			return nil, fmt.Errorf("cache %v :%w", name, err)
		}

		s.Config.Caches = append(s.Config.Caches, CacheConfig{
			Name:     name,
			Cache:    cache,
			Location: loc,
		})
	}

	return s, nil
}
