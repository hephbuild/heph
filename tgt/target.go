package tgt

import (
	"errors"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/xfs"
)

type OutNamedPaths = NamedPaths[xfs.RelPaths, xfs.RelPath]

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
}

func (t *Target) TGTTarget() *Target {
	return t
}

type Targeter interface {
	specs.Specer
	TGTTarget() *Target
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

var ErrStopWalk = errors.New("stop walk")
