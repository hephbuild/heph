package tgt

import (
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/xfs"
	"strings"
)

type TargetWithOutput struct {
	Target     *Target
	Output     string
	SpecOutput string
	Mode       specs.DepMode
}

func (t TargetWithOutput) Full() string {
	if t.Output == "" {
		return t.Target.FQN
	}

	return t.Target.FQN + "|" + t.Output
}

type TargetDeps struct {
	Targets []TargetWithOutput
	Files   []xfs.Path
}

func (d TargetDeps) Merge(deps TargetDeps) TargetDeps {
	nd := TargetDeps{}
	nd.Targets = ads.DedupAppend(d.Targets, func(t TargetWithOutput) string {
		return t.Full()
	}, deps.Targets...)
	nd.Files = ads.DedupAppend(d.Files, func(path xfs.Path) string {
		return path.RelRoot()
	}, deps.Files...)

	return nd
}

func (d *TargetDeps) Dedup() {
	d.Targets = ads.Dedup(d.Targets, func(t TargetWithOutput) string {
		return t.Full()
	})
	d.Files = ads.Dedup(d.Files, func(path xfs.Path) string {
		return path.RelRoot()
	})
}

func (d TargetDeps) Sort() {
	ads.SortP(d.Targets,
		func(i, j *TargetWithOutput) int {
			return strings.Compare(i.Target.FQN, j.Target.FQN)
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
