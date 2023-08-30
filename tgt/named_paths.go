package tgt

import (
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/xfs"
	"golang.org/x/exp/slices"
	"strings"
)

type NamedPaths[TS ~[]T, T xfs.RelablePath] struct {
	names  []string
	named  map[string]TS
	namedm map[string]map[string]struct{}
	all    TS
	allm   map[string]struct{}
}

func (tp *NamedPaths[TS, T]) Named() map[string]TS {
	return tp.named
}

func (tp *NamedPaths[TS, T]) All() TS {
	return tp.all
}

func (tp *NamedPaths[TS, T]) HasName(name string) bool {
	for _, n := range tp.names {
		if n == name {
			return true
		}
	}

	return false
}

func (tp *NamedPaths[TS, T]) Name(name string) TS {
	if tp.named == nil {
		return nil
	}

	return tp.named[name]
}

func (tp *NamedPaths[TS, T]) Names() []string {
	return tp.names
}

func (tp *NamedPaths[TS, T]) IsNamed() bool {
	names := tp.Names()

	return len(names) != 1 || names[0] != ""
}

func (tp *NamedPaths[TS, T]) ProvisionName(name string) {
	if tp.named == nil {
		tp.named = map[string]TS{}
	}
	if tp.namedm == nil {
		tp.namedm = map[string]map[string]struct{}{}
	}

	if _, ok := tp.named[name]; !ok {
		tp.names = append(tp.names, name)
		tp.namedm[name] = map[string]struct{}{}
		tp.named[name] = make(TS, 0)
	}
}

func (tp *NamedPaths[TS, T]) AddAll(name string, ps []T) {
	tp.ProvisionName(name)
	for _, p := range ps {
		tp.Add(name, p)
	}
}

func (tp *NamedPaths[TS, T]) Add(name string, p T) {
	tp.ProvisionName(name)
	if _, ok := tp.namedm[name][p.RelRoot()]; !ok {
		tp.namedm[name][p.RelRoot()] = struct{}{}
		tp.named[name] = append(tp.named[name], p)
	}

	if tp.allm == nil {
		tp.allm = map[string]struct{}{}
	}

	prr := p.RelRoot()
	if _, ok := tp.allm[prr]; !ok {
		tp.all = append(tp.all, p)
		tp.allm[prr] = struct{}{}
	}
}

func (tp *NamedPaths[TS, T]) Sort() {
	slices.SortFunc(tp.all, func(a, b T) int {
		return strings.Compare(a.RelRoot(), b.RelRoot())
	})

	for name := range tp.named {
		slices.SortFunc(tp.named[name], func(a, b T) int {
			return strings.Compare(a.RelRoot(), b.RelRoot())
		})
	}
}

func (tp NamedPaths[TS, T]) withRoot(paths []T, root string) xfs.Paths {
	return ads.Map(paths, func(path T) xfs.Path {
		return path.WithRoot(root)
	})
}

func (tp NamedPaths[TS, T]) WithRoot(root string) *NamedPaths[xfs.Paths, xfs.Path] {
	ntp := &NamedPaths[xfs.Paths, xfs.Path]{
		names: tp.names,
		named: map[string]xfs.Paths{},
		all:   tp.withRoot(tp.all, root),
	}

	for name, paths := range tp.named {
		ntp.named[name] = tp.withRoot(paths, root)
	}

	return ntp
}
