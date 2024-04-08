package worker2

import (
	"fmt"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/sets"
	"github.com/hephbuild/heph/utils/xtypes"
	"strings"
	"sync"
)

func NewDeps(deps ...Dep) *Deps {
	return newDeps(deps)
}

func newDeps(deps []Dep) *Deps {
	d := &Deps{
		deps:                sets.NewIdentitySet[Dep](0),
		transitiveDeps:      sets.NewIdentitySet[Dep](0),
		dependees:           sets.NewIdentitySet[*Deps](0),
		transitiveDependees: sets.NewIdentitySet[*Deps](0),
	}
	d.Add(deps...)
	return d
}

type Deps struct {
	owner               Dep
	deps                *sets.Set[Dep, Dep]
	transitiveDeps      *sets.Set[Dep, Dep]
	dependees           *sets.Set[*Deps, *Deps]
	transitiveDependees *sets.Set[*Deps, *Deps]
	m                   sync.RWMutex
	frozen              bool
}

func (d *Deps) setOwner(dep Dep) {
	if d.owner != nil && d.owner != dep {
		panic("deps owner is already set")
	}
	d.owner = dep
}

func (d *Deps) IsFrozen() bool {
	return d.frozen
}

func (d *Deps) Freeze() {
	d.m.Lock()
	defer d.m.Unlock()

	if d.frozen {
		return
	}

	for _, dep := range d.deps.Slice() {
		if !dep.GetDepsObj().IsFrozen() {
			panic(fmt.Sprintf("attempting to freeze '%v' while all deps aren't frozen, '%v' isnt", d.owner.GetName(), dep.GetName()))
		}
	}

	d.frozen = true
}

func (d *Deps) Dependencies() []Dep {
	d.m.RLock()
	defer d.m.RUnlock()

	return d.deps.Slice()
}

func (d *Deps) Dependees() []*Deps {
	d.m.RLock()
	defer d.m.RUnlock()

	return d.dependees.Slice()
}

func (d *Deps) TransitiveDependencies() []Dep {
	d.m.RLock()
	defer d.m.RUnlock()

	return d.transitiveDeps.Slice()
}

func (d *Deps) TransitiveDependees() []*Deps {
	d.m.RLock()
	defer d.m.RUnlock()

	return d.transitiveDependees.Slice()
}

func (d *Deps) flattenNamed(deps []Dep) []Dep {
	fdeps := ads.Copy(deps)
	for i, tdep := range deps {
		fdeps[i] = flattenNamed(tdep)
	}
	return fdeps
}
func (d *Deps) Add(deps ...Dep) {
	d.m.Lock()
	defer d.m.Unlock()

	if d.frozen {
		panic("add: deps is frozen")
	}

	for _, dep := range deps {
		if xtypes.IsNil(dep) {
			continue
		}
		if !d.has(dep) {
			if dep.GetDepsObj().transitiveDeps.Has(d.owner) {
				panic("cycle")
			}

			d.deps.Add(dep)
			d.transitiveDeps.Add(dep)
			d.transitiveDeps.AddAll(d.flattenNamed(dep.GetDepsObj().transitiveDeps.Slice()))

			for _, dependee := range d.transitiveDependees.Slice() {
				dependee.transitiveDeps.Add(dep)
				dependee.transitiveDeps.AddAll(d.flattenNamed(dep.GetDepsObj().transitiveDeps.Slice()))
			}
		}

		{
			depObj := dep.GetDepsObj()

			if !depObj.hasDependee(d) {
				depObj.dependees.Add(d)

				for _, dep := range depObj.transitiveDeps.Slice() {
					depObj := dep.GetDepsObj()
					depObj.transitiveDependees.AddAll(d.transitiveDependees.Slice())
					depObj.transitiveDependees.Add(d)
				}

				depObj.transitiveDependees.AddAll(d.transitiveDependees.Slice())
				depObj.transitiveDependees.Add(d)
			}
		}
	}
}

// Remove is quite a cold path, expected to be used only to track currently running actions
func (d *Deps) Remove(dep Dep) {
	d.m.Lock()
	defer d.m.Unlock()

	if d.frozen {
		panic("remove: deps is frozen")
	}

	if d.has(dep) {
		d.deps.Remove(dep)
		d.transitiveDeps = d.computeTransitiveDeps()

		for _, dependee := range d.transitiveDependees.Slice() {
			dependee.m.Lock()
			dependee.transitiveDeps = dependee.computeTransitiveDeps()
			dependee.m.Unlock()
		}
	}

	depObj := dep.GetDepsObj()

	if depObj.hasDependee(d) {
		depObj.m.Lock()
		depObj.dependees.Remove(d)
		depObj.transitiveDependees = depObj.computeTransitiveDependees()

		for _, dep := range depObj.transitiveDeps.Slice() {
			dep.GetDepsObj().transitiveDependees = dep.GetDepsObj().computeTransitiveDependees()
		}
		depObj.m.Unlock()
	}
}

func (d *Deps) computeTransitiveDeps() *sets.Set[Dep, Dep] {
	s := sets.NewIdentitySet[Dep](0)
	for _, dep := range d.deps.Slice() {
		s.Add(dep)
		s.AddAll(d.flattenNamed(dep.GetDepsObj().transitiveDeps.Slice()))
	}
	return s
}

func (d *Deps) computeTransitiveDependees() *sets.Set[*Deps, *Deps] {
	s := sets.NewIdentitySet[*Deps](0)
	for _, dep := range d.dependees.Slice() {
		s.Add(dep)
		s.AddAll(dep.transitiveDependees.Slice())
	}
	return s
}

func (d *Deps) has(dep Dep) bool {
	return d.deps.Has(dep) || d.deps.Has(flattenNamed(dep))
}

func (d *Deps) hasDependee(dep *Deps) bool {
	return d.dependees.Has(dep)
}

func (d *Deps) DebugString() string {
	var sb strings.Builder
	fmt.Fprintf(&sb, "%v:\n", d.owner.GetName())
	deps := ads.Map(d.deps.Slice(), Dep.GetName)
	tdeps := ads.Map(d.transitiveDeps.Slice(), Dep.GetName)
	fmt.Fprintf(&sb, "  deps: %v\n", deps)
	fmt.Fprintf(&sb, "  tdeps: %v\n", tdeps)

	depdees := ads.Map(d.dependees.Slice(), func(d *Deps) string {
		return d.owner.GetName()
	})
	tdepdees := ads.Map(d.transitiveDependees.Slice(), func(d *Deps) string {
		return d.owner.GetName()
	})
	fmt.Fprintf(&sb, "  depdees: %v\n", depdees)
	fmt.Fprintf(&sb, "  tdepdees: %v\n", tdepdees)

	return sb.String()
}
