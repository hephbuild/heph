package graph

import (
	"github.com/hephbuild/heph/utils/ads"
)

type TargetTransitive struct {
	Tools          TargetTools     `json:"-"`
	Deps           TargetNamedDeps `json:"-"`
	HashDeps       TargetDeps      `json:"-"`
	RuntimeDeps    TargetNamedDeps `json:"-"`
	Env            map[string]string
	RuntimeEnv     map[string]TargetRuntimeEnv
	PassEnv        []string
	RuntimePassEnv []string
}

func (tr TargetTransitive) Merge(otr TargetTransitive) TargetTransitive {
	ntr := TargetTransitive{
		Env:        map[string]string{},
		RuntimeEnv: map[string]TargetRuntimeEnv{},
	}

	ntr.Tools = tr.Tools
	if !otr.Tools.Empty() {
		ntr.Tools = ntr.Tools.Merge(otr.Tools)
	}

	ntr.Deps = tr.Deps
	if !otr.Deps.Empty() {
		ntr.Deps = ntr.Deps.Merge(otr.Deps)
	}

	ntr.HashDeps = tr.HashDeps
	if !otr.HashDeps.Empty() {
		ntr.HashDeps = ntr.HashDeps.Merge(otr.HashDeps)
	}

	ntr.RuntimeDeps = tr.RuntimeDeps
	if !otr.RuntimeDeps.Empty() {
		ntr.RuntimeDeps = ntr.RuntimeDeps.Merge(otr.RuntimeDeps)
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

	ntr.PassEnv = ads.DedupAppendIdentity(tr.PassEnv, otr.PassEnv...)
	ntr.RuntimePassEnv = ads.DedupAppendIdentity(tr.RuntimePassEnv, otr.RuntimePassEnv...)

	return ntr
}

func (tr TargetTransitive) Empty() bool {
	return tr.Tools.Empty() &&
		tr.Deps.Empty() &&
		tr.HashDeps.Empty() &&
		tr.RuntimeDeps.Empty() &&
		len(tr.Env) == 0 &&
		len(tr.RuntimeEnv) == 0 &&
		len(tr.PassEnv) == 0 &&
		len(tr.RuntimePassEnv) == 0
}
