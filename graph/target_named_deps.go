package graph

import (
	"github.com/hephbuild/heph/utils/sets"
	"golang.org/x/exp/slices"
)

type TargetNamedDeps struct {
	named map[string]TargetDeps
	all   TargetDeps
	names *sets.StringSet
}

func (tp *TargetNamedDeps) Add(name string, p TargetDeps) {
	if tp.named == nil {
		tp.named = map[string]TargetDeps{}
	}
	if tp.names == nil {
		tp.names = sets.NewStringSet(1)
	}

	ep := tp.named[name]
	ep = ep.Merge(p)
	tp.named[name] = ep

	tp.all = tp.all.Merge(p)
	tp.all.Sort()

	tp.names.Add(name)
	slices.Sort(tp.names.Slice())
}

func (tp *TargetNamedDeps) Set(name string, p TargetDeps) {
	if tp.named == nil {
		tp.named = map[string]TargetDeps{}
	}
	if tp.names == nil {
		tp.names = sets.NewStringSet(1)
	}

	tp.named[name] = p

	tp.all = tp.all.Merge(p)
	tp.all.Sort()

	tp.names.Add(name)
	slices.Sort(tp.names.Slice())
}

func (tp *TargetNamedDeps) IsNamed() bool {
	names := tp.Names()

	return len(names) != 1 || names[0] != ""
}

func (tp *TargetNamedDeps) Named() map[string]TargetDeps {
	return tp.named
}

func (tp *TargetNamedDeps) All() TargetDeps {
	return tp.all
}

func (tp *TargetNamedDeps) HasName(name string) bool {
	if tp.named == nil {
		return false
	}

	return tp.names.Has(name)
}

func (tp *TargetNamedDeps) Name(name string) TargetDeps {
	if tp.named == nil {
		return TargetDeps{}
	}

	return tp.named[name]
}

func (tp *TargetNamedDeps) Names() []string {
	return tp.names.Slice()
}

func (tp *TargetNamedDeps) Map(fn func(deps TargetDeps) TargetDeps) {
	for name, deps := range tp.named {
		tp.named[name] = fn(deps)
	}

	tp.all = fn(tp.all)
}

func (tp *TargetNamedDeps) Dedup() {
	tp.Map(func(d TargetDeps) TargetDeps {
		d.Dedup()
		return d
	})
}

func (tp *TargetNamedDeps) Sort() {
	tp.Map(func(deps TargetDeps) TargetDeps {
		deps.Sort()
		return deps
	})
}

func (tp TargetNamedDeps) Copy() TargetNamedDeps {
	ntp := TargetNamedDeps{}
	for name, deps := range tp.Named() {
		ntp.Set(name, deps.Copy())
	}

	return ntp
}

func (tp *TargetNamedDeps) Merge(deps TargetNamedDeps) TargetNamedDeps {
	ntp := TargetNamedDeps{}
	for name, deps := range tp.Named() {
		ntp.Set(name, deps)
	}

	for name, deps := range deps.Named() {
		ntp.Add(name, deps)
	}

	return ntp
}

func (tp *TargetNamedDeps) Empty() bool {
	return tp.All().Empty()
}
