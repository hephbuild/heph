package worker2

import (
	"fmt"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/sets"
	"github.com/hephbuild/heph/utils/xtypes"
	"strings"
	"sync"
)

type Node[T any] struct {
	V  T
	ID string

	m                      sync.RWMutex
	dependencies           *sets.Set[*Node[T], *Node[T]]
	dependees              *sets.Set[*Node[T], *Node[T]]
	transitiveDependencies *sets.Set[*Node[T], *Node[T]]
	transitiveDependees    *sets.Set[*Node[T], *Node[T]]
	frozen                 bool
}

func NewNode[T any](id string, v T) *Node[T] {
	return &Node[T]{
		ID:                     id,
		V:                      v,
		dependencies:           sets.NewIdentitySet[*Node[T]](0),
		dependees:              sets.NewIdentitySet[*Node[T]](0),
		transitiveDependencies: sets.NewIdentitySet[*Node[T]](0),
		transitiveDependees:    sets.NewIdentitySet[*Node[T]](0),
	}
}

func (d *Node[T]) GetID() string {
	return d.ID
}

func (d *Node[T]) AddDependency(deps ...*Node[T]) {
	d.m.Lock()
	defer d.m.Unlock()

	if d.frozen {
		panic("add: frozen")
	}

	for _, dep := range deps {
		d.addDependency(dep)
	}
}

func (d *Node[T]) addDependency(dep *Node[T]) {
	if xtypes.IsNil(dep) {
		return
	}

	if !d.dependencies.Has(dep) {
		if dep.transitiveDependencies.Has(d) {
			panic("cycle")
		}

		d.dependencies.Add(dep)
		d.transitiveDependencies.Add(dep)
		d.transitiveDependencies.AddAll(dep.transitiveDependencies.Slice())

		for _, dependee := range d.transitiveDependees.Slice() {
			dependee.transitiveDependencies.Add(dep)
			dependee.transitiveDependencies.AddAll(dep.transitiveDependencies.Slice())
		}
	}

	{
		if !dep.dependees.Has(d) {
			dep.dependees.Add(d)

			for _, dep := range dep.transitiveDependencies.Slice() {
				dep.transitiveDependees.AddAll(d.transitiveDependees.Slice())
				dep.transitiveDependees.Add(d)
			}

			dep.transitiveDependees.AddAll(d.transitiveDependees.Slice())
			dep.transitiveDependees.Add(d)
		}
	}
}

func (d *Node[T]) RemoveDependency(dep *Node[T]) {
	d.m.Lock()
	defer d.m.Unlock()

	if d.frozen {
		panic("remove: deps is frozen")
	}

	if d.dependencies.Has(dep) {
		d.dependencies.Remove(dep)
		d.transitiveDependencies = d.computeTransitiveDeps()

		for _, dependee := range d.transitiveDependees.Slice() {
			dependee.m.Lock()
			dependee.transitiveDependencies = dependee.computeTransitiveDeps()
			dependee.m.Unlock()
		}
	}

	if dep.dependees.Has(d) {
		dep.m.Lock()
		dep.dependees.Remove(d)
		dep.transitiveDependees = dep.computeTransitiveDependees()
		dep.m.Unlock()

		for _, dep := range dep.transitiveDependencies.Slice() {
			dep.m.Lock()
			dep.transitiveDependees = dep.computeTransitiveDependees()
			dep.m.Unlock()
		}
	}
}

func (d *Node[T]) computeTransitiveDeps() *sets.Set[*Node[T], *Node[T]] {
	s := sets.NewIdentitySet[*Node[T]](0)
	for _, dep := range d.dependencies.Slice() {
		s.Add(dep)
		s.AddAll(dep.transitiveDependencies.Slice())
	}
	return s
}

func (d *Node[T]) computeTransitiveDependees() *sets.Set[*Node[T], *Node[T]] {
	s := sets.NewIdentitySet[*Node[T]](0)
	for _, dep := range d.dependees.Slice() {
		s.Add(dep)
		s.AddAll(dep.transitiveDependees.Slice())
	}
	return s
}

func (d *Node[T]) IsFrozen() bool {
	return d.frozen
}

func (d *Node[T]) Freeze() {
	d.m.Lock()
	defer d.m.Unlock()

	if d.frozen {
		return
	}

	for _, dep := range d.dependencies.Slice() {
		if !dep.IsFrozen() {
			panic(fmt.Sprintf("attempting to freeze '%v' while all deps aren't frozen, '%v' isnt", d.ID, dep.ID))
		}
	}

	d.frozen = true
}

func (d *Node[T]) toV(nodes []*Node[T]) []T {
	return ads.Map(nodes, func(t *Node[T]) T {
		return t.V
	})
}

func (d *Node[T]) Dependencies() []T {
	d.m.RLock()
	defer d.m.RUnlock()

	return d.toV(d.dependencies.Slice())
}

func (d *Node[T]) Dependees() []T {
	d.m.RLock()
	defer d.m.RUnlock()

	return d.toV(d.dependees.Slice())
}

func (d *Node[T]) TransitiveDependencies() []T {
	d.m.RLock()
	defer d.m.RUnlock()

	return d.toV(d.transitiveDependencies.Slice())
}

func (d *Node[T]) TransitiveDependees() []T {
	d.m.RLock()
	defer d.m.RUnlock()

	return d.toV(d.transitiveDependees.Slice())
}

func (d *Node[T]) DebugString() string {
	var sb strings.Builder
	fmt.Fprintf(&sb, "%v:\n", d.ID)
	deps := ads.Map(d.dependencies.Slice(), (*Node[T]).GetID)
	tdeps := ads.Map(d.transitiveDependencies.Slice(), (*Node[T]).GetID)
	fmt.Fprintf(&sb, "  deps: %v\n", deps)
	fmt.Fprintf(&sb, "  tdeps: %v\n", tdeps)

	depdees := ads.Map(d.dependees.Slice(), (*Node[T]).GetID)
	tdepdees := ads.Map(d.transitiveDependees.Slice(), (*Node[T]).GetID)
	fmt.Fprintf(&sb, "  depdees: %v\n", depdees)
	fmt.Fprintf(&sb, "  tdepdees: %v\n", tdepdees)

	return sb.String()
}
