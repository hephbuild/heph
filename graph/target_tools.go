package graph

import (
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/xfs"
	"strings"
)

type TargetTool struct {
	Target *Target
	Output string
	Name   string
	File   xfs.RelPath
}

type targetToolComparable struct {
	name, addr, output string
}

func (t TargetTool) comparable() targetToolComparable {
	return targetToolComparable{t.Name, t.Target.Addr, t.Output}
}

func (t TargetTool) Full() string {
	if t.Output != "" {
		return t.Target.Addr + "|" + t.Output
	}

	return t.Target.Addr
}

type TargetTools struct {
	// Holds targets references that do not have output (for transitive for ex)
	TargetReferences []*Target
	Targets          []TargetTool
	Hosts            []specs.HostTool
}

func (t TargetTools) HasHeph() bool {
	for _, tool := range t.Hosts {
		if tool.BinName == "heph" {
			return true
		}
	}

	return false
}

func (t TargetTools) Merge(tools TargetTools) TargetTools {
	tt := TargetTools{}
	tt.TargetReferences = ads.DedupAppend(t.TargetReferences, func(t *Target) string {
		return t.Addr
	}, tools.TargetReferences...)
	tt.Targets = ads.DedupAppend(t.Targets, TargetTool.comparable, tools.Targets...)
	tt.Hosts = ads.DedupAppendIdentity(t.Hosts, tools.Hosts...)

	return tt
}

func (t TargetTools) Empty() bool {
	return len(t.Targets) == 0 && len(t.Hosts) == 0 && len(t.TargetReferences) == 0
}

func (t *TargetTools) Dedup() {
	t.TargetReferences = ads.Dedup(t.TargetReferences, func(target *Target) string {
		return target.Addr
	})
	t.Targets = ads.Dedup(t.Targets, TargetTool.comparable)
	t.Hosts = ads.DedupIdentity(t.Hosts)
}

func (t TargetTools) Sort() {
	ads.SortP(t.Hosts,
		func(i, j *specs.HostTool) int {
			return strings.Compare(i.BinName, j.BinName)
		},
		func(i, j *specs.HostTool) int {
			return strings.Compare(i.Name, j.Name)
		},
	)

	ads.SortP(t.Targets,
		func(i, j *TargetTool) int {
			return strings.Compare(i.Target.Addr, j.Target.Addr)
		},
		func(i, j *TargetTool) int {
			return strings.Compare(i.Name, j.Name)
		},
	)

	ads.Sort(t.TargetReferences,
		func(i, j *Target) int {
			return strings.Compare(i.Addr, j.Addr)
		},
		func(i, j *Target) int {
			return strings.Compare(i.Name, j.Name)
		},
	)
}
