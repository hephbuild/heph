package tgt

import (
	"errors"
	"heph/targetspec"
	"heph/utils/fs"
)

type OutNamedPaths = NamedPaths[fs.RelPaths, fs.RelPath]

type Target struct {
	targetspec.TargetSpec

	StoreUID uint64

	Tools          TargetTools
	Deps           TargetNamedDeps
	HashDeps       TargetDeps
	OutWithSupport *OutNamedPaths
	Out            *OutNamedPaths
	Env            map[string]string
	RuntimeEnv     map[string]TargetRuntimeEnv
	RuntimePassEnv []string

	// Collected transitive deps from deps/tools
	TransitiveDeps TargetTransitive

	// Own transitive config
	OwnTransitive TargetTransitive
	// Own transitive config plus their own transitive
	DeepOwnTransitive TargetTransitive
}

type TargetRuntimeEnv struct {
	Value  string
	Target *Target
}

func (t *Target) GetUID() uint64 {
	if t.StoreUID == 0 {
		panic("SetId should not be 0")
	}

	return t.StoreUID
}

func (t *Target) ToolTarget() TargetTool {
	if !t.IsTool() {
		panic("not a tool target")
	}

	ts := t.TargetSpec.Tools.Targets[0]

	for _, tool := range t.Tools.Targets {
		if tool.Target.FQN == ts.Target {
			return tool
		}
	}

	panic("unable to find tool target bin")
}

var ErrStopWalk = errors.New("stop walk")
