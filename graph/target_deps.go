package graph

import (
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/xfs"
	"strings"
)

type TargetWithOutput struct {
	Target *Target
	Output string
	Mode   specs.DepMode
	Name   string
}

type TargetWithOutputComparable struct {
	addr, output string
}

func (t TargetWithOutput) Comparable() TargetWithOutputComparable {
	return TargetWithOutputComparable{t.Target.Addr, t.Output}
}

func (t TargetWithOutput) Full() string {
	if t.Output == "" {
		return t.Target.Addr
	}

	return t.Target.Addr + "|" + t.Output
}

type TargetDeps struct {
	Targets    []TargetWithOutput // Targets with groups inlined
	RawTargets []TargetWithOutput // Targets with no inlining
	Files      []xfs.Path
}

func (d TargetDeps) Merge(deps TargetDeps) TargetDeps {
	nd := TargetDeps{}

	nd.Targets = ads.DedupAppend(d.Targets, func(t TargetWithOutput) TargetWithOutputComparable {
		return t.Comparable()
	}, deps.Targets...)
	nd.RawTargets = ads.DedupAppend(d.RawTargets, func(t TargetWithOutput) TargetWithOutputComparable {
		return t.Comparable()
	}, deps.RawTargets...)

	nd.Files = ads.DedupAppend(d.Files, func(path xfs.Path) string {
		return path.RelRoot()
	}, deps.Files...)

	return nd
}

func (d *TargetDeps) Dedup() {
	d.Targets = ads.Dedup(d.Targets, func(t TargetWithOutput) TargetWithOutputComparable {
		return t.Comparable()
	})
	d.RawTargets = ads.Dedup(d.RawTargets, func(t TargetWithOutput) TargetWithOutputComparable {
		return t.Comparable()
	})
	d.Files = ads.Dedup(d.Files, func(path xfs.Path) string {
		return path.RelRoot()
	})
}

func (d TargetDeps) Sort() {
	ads.SortP(d.Targets,
		func(i, j *TargetWithOutput) int {
			return strings.Compare(i.Target.Addr, j.Target.Addr)
		},
		func(i, j *TargetWithOutput) int {
			return strings.Compare(i.Output, j.Output)
		},
	)

	ads.Sort(d.Files, func(i, j xfs.Path) int {
		return strings.Compare(i.RelRoot(), j.RelRoot())
	})
}

func (d TargetDeps) Empty() bool {
	return len(d.Targets) == 0 && len(d.Files) == 0
}
