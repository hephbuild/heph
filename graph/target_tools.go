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
		return t.FQN
	}, tools.TargetReferences...)
	tt.Targets = ads.DedupAppend(t.Targets, func(tool TargetTool) string {
		return tool.Name + "|" + tool.Target.FQN + "|" + tool.Output
	}, tools.Targets...)
	tt.Hosts = ads.DedupAppend(t.Hosts, func(tool specs.HostTool) string {
		return tool.Name + "|" + tool.BinName + "|" + tool.Path
	}, tools.Hosts...)

	return tt
}

func (t TargetTools) Empty() bool {
	return len(t.Targets) == 0 && len(t.Hosts) == 0 && len(t.TargetReferences) == 0
}

func (t TargetTools) Dedup() {
	t.Hosts = ads.Dedup(t.Hosts, func(tool specs.HostTool) string {
		return tool.Name + "|" + tool.BinName + "|" + tool.Path
	})
	t.Targets = ads.Dedup(t.Targets, func(tool TargetTool) string {
		return tool.Name + "|" + tool.Target.FQN + "|" + tool.Output
	})
	t.TargetReferences = ads.Dedup(t.TargetReferences, func(target *Target) string {
		return target.FQN
	})
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
			return strings.Compare(i.Target.FQN, j.Target.FQN)
		},
		func(i, j *TargetTool) int {
			return strings.Compare(i.Name, j.Name)
		},
	)

	ads.Sort(t.TargetReferences,
		func(i, j *Target) int {
			return strings.Compare(i.FQN, j.FQN)
		},
		func(i, j *Target) int {
			return strings.Compare(i.Name, j.Name)
		},
	)
}
