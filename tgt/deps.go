package tgt

import (
	"heph/targetspec"
	"heph/utils"
	"heph/utils/fs"
	"sort"
	"strings"
)

type TargetWithOutput struct {
	Target     *Target
	Output     string
	SpecOutput string
	Mode       targetspec.TargetSpecDepMode
}

func (t TargetWithOutput) Full() string {
	if t.Output == "" {
		return t.Target.FQN
	}

	return t.Target.FQN + "|" + t.Output
}

type TargetDeps struct {
	Targets []TargetWithOutput
	Files   []fs.Path
}

func (d TargetDeps) Merge(deps TargetDeps) TargetDeps {
	nd := TargetDeps{}
	nd.Targets = utils.DedupAppend(d.Targets, func(t TargetWithOutput) string {
		return t.Full()
	}, deps.Targets...)
	nd.Files = utils.DedupAppend(d.Files, func(path fs.Path) string {
		return path.RelRoot()
	}, deps.Files...)

	return nd
}

func (d *TargetDeps) Dedup() {
	d.Targets = utils.Dedup(d.Targets, func(t TargetWithOutput) string {
		return t.Full()
	})
	d.Files = utils.Dedup(d.Files, func(path fs.Path) string {
		return path.RelRoot()
	})
}

func (d TargetDeps) Sort() {
	sort.Slice(d.Targets, utils.MultiLess(
		func(i, j int) int {
			return strings.Compare(d.Targets[i].Target.FQN, d.Targets[j].Target.FQN)
		},
		func(i, j int) int {
			return strings.Compare(d.Targets[i].Output, d.Targets[j].Output)
		},
	))

	sort.Slice(d.Files, func(i, j int) bool {
		return d.Files[i].RelRoot() < d.Files[j].RelRoot()
	})
}
