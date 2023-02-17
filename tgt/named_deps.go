package tgt

import "sort"

type TargetNamedDeps struct {
	named map[string]TargetDeps
	all   TargetDeps
}

func (tp *TargetNamedDeps) Set(name string, p TargetDeps) {
	if tp.named == nil {
		tp.named = map[string]TargetDeps{}
	}

	tp.named[name] = p
	tp.all = tp.all.Merge(p)
}

func (tp *TargetNamedDeps) Named() map[string]TargetDeps {
	return tp.named
}

func (tp *TargetNamedDeps) All() TargetDeps {
	return tp.all
}

func (tp *TargetNamedDeps) Name(name string) TargetDeps {
	if tp.named == nil {
		return TargetDeps{}
	}

	return tp.named[name]
}

func (tp *TargetNamedDeps) Names() []string {
	names := make([]string, 0, len(tp.named))
	for name := range tp.named {
		names = append(names, name)
	}
	sort.Strings(names)
	return names
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

func (tp *TargetNamedDeps) Merge(deps TargetNamedDeps) TargetNamedDeps {
	ntp := TargetNamedDeps{}
	for name, deps := range tp.Named() {
		ntp.Set(name, deps)
	}

	for name, deps := range deps.Named() {
		ntp.Set(name, ntp.Name(name).Merge(deps))
	}

	return ntp
}

func (tp *TargetNamedDeps) Empty() bool {
	return len(tp.All().Targets) == 0 && len(tp.All().Files) == 0
}
