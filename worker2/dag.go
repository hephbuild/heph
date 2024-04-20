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
	V      T
	ID     string
	frozen bool
	m      sync.Mutex

	mdependencies               sync.RWMutex
	dependencies                *sets.Set[*Node[T], *Node[T]]
	transitiveDependencies      *sets.Set[*Node[T], *Node[T]]
	transitiveDependenciesDirty bool

	mdependees               sync.RWMutex
	dependees                *sets.Set[*Node[T], *Node[T]]
	transitiveDependees      *sets.Set[*Node[T], *Node[T]]
	transitiveDependeesDirty bool
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
		if dep.HasTransitiveDependency(d) {
			panic("cycle")
		}

		d.dependencies.Add(dep)
		d.transitiveDependenciesDirty = true

		for _, dependee := range d.TransitiveDependeesNodes() {
			dependee.transitiveDependenciesDirty = true
		}
	}

	{
		if !dep.dependees.Has(d) {
			dep.dependees.Add(d)

			for _, dep := range dep.TransitiveDependenciesNodes() {
				dep.transitiveDependeesDirty = true
			}

			dep.transitiveDependeesDirty = true
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
		d.transitiveDependencies = nil

		for _, dependee := range d.TransitiveDependeesNodes() {
			dependee.mdependencies.Lock()
			dependee.transitiveDependencies = nil
			dependee.mdependencies.Unlock()
		}
	}

	if dep.dependees.Has(d) {
		dep.mdependees.Lock()
		dep.dependees.Remove(d)
		dep.transitiveDependees = nil
		dep.mdependees.Unlock()

		for _, dep := range dep.TransitiveDependenciesNodes() {
			dep.mdependees.Lock()
			dep.transitiveDependees = nil
			dep.mdependees.Unlock()
		}
	}
}

func (d *Node[T]) computeTransitiveDependencies(full bool) *sets.Set[*Node[T], *Node[T]] {
	s := d.transitiveDependencies
	if full {
		s = sets.NewIdentitySet[*Node[T]](0)
	}
	for _, dep := range d.dependencies.Slice() {
		s.Add(dep)
		s.AddAll(dep.TransitiveDependenciesNodes())
	}
	return s
}

func (d *Node[T]) computeTransitiveDependees(full bool) *sets.Set[*Node[T], *Node[T]] {
	s := d.transitiveDependees
	if full {
		s = sets.NewIdentitySet[*Node[T]](0)
	}
	for _, dep := range d.dependees.Slice() {
		s.AddAll(dep.TransitiveDependeesNodes())
		s.Add(dep)
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

func (d *Node[T]) DependenciesNodes() []*Node[T] {
	d.mdependencies.RLock()
	defer d.mdependencies.RUnlock()

	return d.dependencies.Slice()
}

func (d *Node[T]) Dependencies() []T {
	return d.toV(d.DependenciesNodes())
}

func (d *Node[T]) DependeesNodes() []*Node[T] {
	d.mdependees.RLock()
	defer d.mdependees.RUnlock()

	return d.dependees.Slice()
}

func (d *Node[T]) Dependees() []T {
	return d.toV(d.DependeesNodes())
}

func (d *Node[T]) transitiveDependenciesSet() *sets.Set[*Node[T], *Node[T]] {
	if d.transitiveDependencies == nil {
		d.transitiveDependencies = d.computeTransitiveDependencies(true)
	} else if d.transitiveDependenciesDirty {
		d.transitiveDependencies = d.computeTransitiveDependencies(false)
	}
	d.transitiveDependenciesDirty = false

	return d.transitiveDependencies
}

func (d *Node[T]) TransitiveDependenciesNodes() []*Node[T] {
	d.mdependencies.Lock()
	defer d.mdependencies.Unlock()

	return d.transitiveDependenciesSet().Slice()
}

func (d *Node[T]) TransitiveDependencies() []T {
	return d.toV(d.TransitiveDependenciesNodes())
}

func (d *Node[T]) HasTransitiveDependency(dep *Node[T]) bool {
	d.mdependencies.Lock()
	defer d.mdependencies.Unlock()

	return d.transitiveDependenciesSet().Has(dep)
}

func (d *Node[T]) TransitiveDependeesNodes() []*Node[T] {
	d.mdependees.Lock()
	defer d.mdependees.Unlock()

	if d.transitiveDependees == nil {
		d.transitiveDependees = d.computeTransitiveDependees(true)
	} else if d.transitiveDependeesDirty {
		d.transitiveDependees = d.computeTransitiveDependees(false)
	}
	d.transitiveDependeesDirty = false

	return d.transitiveDependees.Slice()
}

func (d *Node[T]) TransitiveDependees() []T {
	return d.toV(d.TransitiveDependeesNodes())
}

func (d *Node[T]) DebugString() string {
	var sb strings.Builder
	fmt.Fprintf(&sb, "%v:\n", d.ID)
	deps := ads.Map(d.DependenciesNodes(), (*Node[T]).GetID)
	tdeps := ads.Map(d.TransitiveDependenciesNodes(), (*Node[T]).GetID)
	fmt.Fprintf(&sb, "  deps: %v\n", deps)
	fmt.Fprintf(&sb, "  tdeps: %v\n", tdeps)

	depdees := ads.Map(d.DependeesNodes(), (*Node[T]).GetID)
	tdepdees := ads.Map(d.TransitiveDependeesNodes(), (*Node[T]).GetID)
	fmt.Fprintf(&sb, "  depdees: %v\n", depdees)
	fmt.Fprintf(&sb, "  tdepdees: %v\n", tdepdees)

	return sb.String()
}
