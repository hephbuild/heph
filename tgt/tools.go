package tgt

import (
	"heph/targetspec"
	"heph/utils"
	"heph/utils/fs"
	"sort"
	"strings"
)

type TargetTool struct {
	Target *Target
	Output string
	Name   string
	File   fs.RelPath
}

type TargetTools struct {
	// Holds targets references that do not have output (for transitive for ex)
	TargetReferences []*Target
	Targets          []TargetTool
	Hosts            []targetspec.TargetSpecHostTool
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
	tt.TargetReferences = utils.DedupAppend(t.TargetReferences, func(t *Target) string {
		return t.FQN
	}, tools.TargetReferences...)
	tt.Targets = utils.DedupAppend(t.Targets, func(tool TargetTool) string {
		return tool.Name + "|" + tool.Target.FQN + "|" + tool.Output
	}, tools.Targets...)
	tt.Hosts = utils.DedupAppend(t.Hosts, func(tool targetspec.TargetSpecHostTool) string {
		return tool.Name + "|" + tool.BinName + "|" + tool.Path
	}, tools.Hosts...)

	return tt
}

func (t TargetTools) Empty() bool {
	return len(t.Targets) == 0 && len(t.Hosts) == 0 && len(t.TargetReferences) == 0
}

func (t TargetTools) Dedup() {
	t.Hosts = utils.Dedup(t.Hosts, func(tool targetspec.TargetSpecHostTool) string {
		return tool.Name + "|" + tool.BinName + "|" + tool.Path
	})
	t.Targets = utils.Dedup(t.Targets, func(tool TargetTool) string {
		return tool.Name + "|" + tool.Target.FQN + "|" + tool.Output
	})
	t.TargetReferences = utils.Dedup(t.TargetReferences, func(target *Target) string {
		return target.FQN
	})
}

func (t TargetTools) Sort() {
	sort.Slice(t.Hosts, utils.MultiLess(
		func(i, j int) int {
			return strings.Compare(t.Hosts[i].BinName, t.Hosts[j].BinName)
		},
		func(i, j int) int {
			return strings.Compare(t.Hosts[i].Name, t.Hosts[j].Name)
		},
	))

	sort.Slice(t.Targets, utils.MultiLess(
		func(i, j int) int {
			return strings.Compare(t.Targets[i].Target.FQN, t.Targets[j].Target.FQN)
		},
		func(i, j int) int {
			return strings.Compare(t.Targets[i].Name, t.Targets[j].Name)
		},
	))

	sort.Slice(t.TargetReferences, utils.MultiLess(
		func(i, j int) int {
			return strings.Compare(t.TargetReferences[i].FQN, t.TargetReferences[j].FQN)
		},
		func(i, j int) int {
			return strings.Compare(t.TargetReferences[i].Name, t.TargetReferences[j].Name)
		},
	))
}
