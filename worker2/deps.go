package worker2

import (
	"fmt"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/sets"
	"strings"
	"sync"
)

type DepHook = func(dep Dep)

func NewDeps(deps ...Dep) *Deps {
	return newDeps("", deps)
}

func NewDepsID(id string, deps ...Dep) *Deps {
	return newDeps(id, deps)
}

func newDeps(id string, deps []Dep) *Deps {
	d := &Deps{
		id:                  id,
		deps:                sets.NewIdentitySet[Dep](0),
		transitiveDeps:      sets.NewIdentitySet[Dep](0),
		dependees:           sets.NewIdentitySet[*Deps](0),
		transitiveDependees: sets.NewIdentitySet[*Deps](0),
	}
	d.Add(deps...)
	return d
}

type Deps struct {
	id                  string
	deps                *sets.Set[Dep, Dep]
	transitiveDeps      *sets.Set[Dep, Dep]
	dependees           *sets.Set[*Deps, *Deps]
	transitiveDependees *sets.Set[*Deps, *Deps]
	m                   sync.RWMutex
	frozen              bool
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
		if !dep.IsFrozen() {
			panic("attempting to freeze while all deps aren't frozen")
		}
	}

	d.frozen = true
}

//func (d *Deps) AddHook(hook DepHook) {
//	d.m.Lock()
//	defer d.m.Unlock()
//
//	d.hooks = append(d.hooks, hook)
//
//	for _, dep := range d.deps {
//		d.runHooks(dep)
//	}
//}

func (d *Deps) Dependencies() []Dep {
	d.m.RLock()
	defer d.m.RUnlock()

	return d.deps.Slice()
}

func (d *Deps) TransitiveDependencies() []Dep {
	d.m.RLock()
	defer d.m.RUnlock()

	return d.transitiveDeps.Slice()
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
		d.m.Unlock()
		panic("deps is frozen")
	}

	for _, dep := range deps {
		if !d.has(dep) {
			d.deps.Add(dep)
			d.transitiveDeps.Add(dep)
			d.transitiveDeps.AddAll(d.flattenNamed(dep.GetDepsObj().transitiveDeps.Slice()))

			for _, dependee := range d.transitiveDependees.Slice() {
				dependee.transitiveDeps.Add(dep)
				dependee.transitiveDeps.AddAll(d.flattenNamed(dep.GetDepsObj().transitiveDeps.Slice()))
			}
		}

		{
			dep := dep.GetDepsObj()

			if !dep.hasDependee(d) {
				dep.dependees.Add(d)

				for _, dep := range dep.transitiveDeps.Slice() {
					dep.GetDepsObj().transitiveDependees.AddAll(d.transitiveDependees.Slice())
					dep.GetDepsObj().transitiveDependees.Add(d)
				}

				dep.transitiveDependees.AddAll(d.transitiveDependees.Slice())
				dep.transitiveDependees.Add(d)
			}
		}

		//addedDeps = append(addedDeps, dep)

		//d.runHooks(dep)
	}

	//for _, dependee := range d.transitiveDependees {
	//	dependee.computeTransitiveDeps()
	//}

	//for _, dep := range addedDeps {
	//	dep.GetDepsObj().AddHook(func(dep Dep) {
	//		d.m.Lock()
	//		defer d.m.Unlock()
	//
	//		d.computeTransitiveDeps()
	//	})
	//}
}

func (d *Deps) has(dep Dep) bool {
	return d.deps.Has(dep) || d.deps.Has(flattenNamed(dep))
}

func (d *Deps) hasDependee(dep *Deps) bool {
	return d.dependees.Has(dep)
}

func (d *Deps) DebugString() string {
	var sb strings.Builder
	fmt.Fprintf(&sb, "%v:\n", d.id)
	deps := ads.Map(d.deps.Slice(), func(d Dep) string {
		return d.GetID()
	})
	tdeps := ads.Map(d.transitiveDeps.Slice(), func(d Dep) string {
		return d.GetID()
	})
	fmt.Fprintf(&sb, "  deps: %v\n", deps)
	fmt.Fprintf(&sb, "  tdeps: %v\n", tdeps)

	depdees := ads.Map(d.dependees.Slice(), func(d *Deps) string {
		return d.id
	})
	tdepdees := ads.Map(d.transitiveDependees.Slice(), func(d *Deps) string {
		return d.id
	})
	fmt.Fprintf(&sb, "  depdees: %v\n", depdees)
	fmt.Fprintf(&sb, "  tdepdees: %v\n", tdepdees)

	return sb.String()
}

//func (d *Deps) runHooks(dep Dep) {
//	for _, hook := range d.hooks {
//		hook(dep)
//	}
//}
