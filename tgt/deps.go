package tgt

import (
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/utils"
	"github.com/hephbuild/heph/utils/fs"
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
	utils.SortP(d.Targets,
		func(i, j *TargetWithOutput) int {
			return strings.Compare(i.Target.FQN, j.Target.FQN)
		},
		func(i, j *TargetWithOutput) int {
			return strings.Compare(i.Output, j.Output)
		},
	)

	utils.Sort(d.Files, func(i, j fs.Path) int {
		return strings.Compare(i.RelRoot(), j.RelRoot())
	})
}
