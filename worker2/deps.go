package worker2

import (
	"golang.org/x/exp/slices"
	"sync"
)

type DepHook = func(dep Dep)

func NewDeps(deps ...Dep) *Deps {
	return newDeps("", deps)
}

func NewDepsID(id string, deps ...Dep) *Deps {
	return newDeps(id, deps)
}

//func NewDepsFrom(deps []Dep) *Deps {
//	return newDeps("", deps)
//}

func newDeps(id string, deps []Dep) *Deps {
	d := &Deps{id: id}
	d.Add(deps...)
	return d
}

type Deps struct {
	id                  string
	deps                []Dep
	depsm               map[Dep]struct{}
	transitiveDeps      []Dep
	dependees           []*Deps
	dependeesm          map[*Deps]struct{}
	transitiveDependees []*Deps
	m                   sync.Mutex
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

	for _, dep := range d.deps {
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
	return d.deps[:]
}

func (d *Deps) TransitiveDependencies() []Dep {
	return d.transitiveDeps[:]
}

func (d *Deps) Dependees() []*Deps {
	return d.dependees[:]
}

func (d *Deps) TransitiveDependees() []*Deps {
	return d.transitiveDependees[:]
}

func (d *Deps) Add(deps ...Dep) {
	d.m.Lock()
	defer d.m.Unlock()

	if d.frozen {
		d.m.Unlock()
		panic("deps is frozen")
	}

	if d.depsm == nil {
		d.depsm = map[Dep]struct{}{}
	}

	if d.dependeesm == nil {
		d.dependeesm = map[*Deps]struct{}{}
	}

	//var addedDeps []Dep
	for _, dep := range deps {

		if !d.has(dep) {
			d.depsm[dep] = struct{}{}
			d.deps = append(d.deps, dep)
			d.computeTransitiveDeps()

			for _, dependee := range d.transitiveDependees {
				dependee.computeTransitiveDeps()
			}
		}

		{
			dep := dep.GetDepsObj()

			if !dep.hasDependee(d) {
				dep.dependeesm[d] = struct{}{}
				dep.dependees = append(dep.dependees, d)
				dep.computeTransitiveDependees()

				for _, dep := range dep.transitiveDeps {
					dep.GetDepsObj().computeTransitiveDependees()
				}
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
	_, ok := d.depsm[dep]
	return ok
}

func (d *Deps) hasDependee(dep *Deps) bool {
	_, ok := d.dependeesm[dep]
	return ok
}

//func (d *Deps) runHooks(dep Dep) {
//	for _, hook := range d.hooks {
//		hook(dep)
//	}
//}

func (d *Deps) computeTransitiveDeps() {
	var transitiveDeps []Dep
	transitiveDeps = append(transitiveDeps, d.deps...)

	for _, dep := range d.deps {
		dep := dep.GetDepsObj()
		dep.computeTransitiveDeps()

		for _, tdep := range dep.transitiveDeps {
			if !slices.Contains(transitiveDeps, tdep) {
				transitiveDeps = append(transitiveDeps, tdep)
			}
		}
	}

	d.transitiveDeps = transitiveDeps
}

//func (d *Deps) computeTransitiveDeps2() {
//
//	var transitiveDeps []Dep
//	d.collectTransitiveDeps(nil, &transitiveDeps)
//	d.transitiveDeps = transitiveDeps
//}
//
//func (d *Deps) collectTransitiveDeps(m map[Dep]struct{}, tdeps *[]Dep) {
//	if m == nil {
//		m = map[Dep]struct{}{}
//	}
//
//	for _, dep := range d.Get() {
//		if _, ok := m[dep]; ok {
//			return
//		}
//		m[dep] = struct{}{}
//
//		dep.GetDepsObj().collectTransitiveDeps(m, tdeps)
//		dep.GetDepsObj().computeTransitiveDeps()
//
//		*tdeps = append(*tdeps, dep)
//	}
//}

func (d *Deps) computeTransitiveDependees() {
	var transitiveDependees []*Deps

	for _, dep := range d.dependees {
		dep.computeTransitiveDependees()

		for _, tdep := range dep.transitiveDependees {
			if !slices.Contains(transitiveDependees, tdep) {
				transitiveDependees = append(transitiveDependees, tdep)
			}
		}
	}

	transitiveDependees = append(transitiveDependees, d.dependees...)

	d.transitiveDependees = transitiveDependees
}

//func (d *Deps) computeTransitiveDependees2() {
//
//	var transitiveDependees []*Deps
//	d.collectTransitiveDependees(nil, &transitiveDependees)
//	d.transitiveDependees = transitiveDependees
//}
//
//func (d *Deps) collectTransitiveDependees(m map[*Deps]struct{}, tdeps *[]*Deps) {
//	if m == nil {
//		m = map[*Deps]struct{}{}
//	}
//
//	for _, dep := range d.dependees {
//		if _, ok := m[dep]; ok {
//			return
//		}
//		m[dep] = struct{}{}
//
//		dep.collectTransitiveDependees(m, tdeps)
//		dep.computeTransitiveDependees()
//
//		*tdeps = append(*tdeps, dep)
//	}
//}
