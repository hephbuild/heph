package graph

import "github.com/hephbuild/heph/specs"

type TargetTransitive struct {
	Tools          TargetTools
	Deps           TargetNamedDeps
	Env            map[string]string
	RuntimeEnv     map[string]TargetRuntimeEnv
	PassEnv        []string
	RuntimePassEnv []string
	Platforms      []specs.Platform
}

func (tr TargetTransitive) Merge(otr TargetTransitive) TargetTransitive {
	ntr := TargetTransitive{
		Env:        map[string]string{},
		RuntimeEnv: map[string]TargetRuntimeEnv{},
	}

	if otr.Tools.Empty() {
		ntr.Tools = tr.Tools
	} else {
		ntr.Tools = tr.Tools.Merge(otr.Tools)
	}

	if otr.Deps.Empty() {
		ntr.Deps = tr.Deps
	} else {
		ntr.Deps = tr.Deps.Merge(otr.Deps)
	}

	for k, v := range tr.Env {
		ntr.Env[k] = v
	}
	for k, v := range otr.Env {
		ntr.Env[k] = v
	}

	for k, v := range tr.RuntimeEnv {
		ntr.RuntimeEnv[k] = v
	}
	for k, v := range otr.RuntimeEnv {
		ntr.RuntimeEnv[k] = v
	}

	ntr.PassEnv = append(tr.PassEnv, otr.PassEnv...)
	ntr.RuntimePassEnv = append(tr.RuntimePassEnv, otr.RuntimePassEnv...)

	return ntr
}

func (tr TargetTransitive) Empty() bool {
	return tr.Tools.Empty() &&
		tr.Deps.Empty() &&
		len(tr.Env) == 0 &&
		len(tr.RuntimeEnv) == 0 &&
		len(tr.PassEnv) == 0 &&
		len(tr.RuntimePassEnv) == 0
}
