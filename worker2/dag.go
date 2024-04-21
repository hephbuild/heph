package worker2

import (
	"fmt"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/sets"
	"strings"
	"sync"
)

type nodesTransitive[T any] struct {
	m                 sync.RWMutex
	nodes             *sets.Set[*Node[T], *Node[T]]
	transitiveNodes   *sets.Set[*Node[T], *Node[T]]
	transitiveDirty   bool
	transitiveGetter  func(d *Node[T]) *nodesTransitive[T]
	transitiveReverse bool
}

func (d *nodesTransitive[T]) Add(dep *Node[T]) {
	d.m.Lock()
	defer d.m.Unlock()

	d.nodes.Add(dep)
	d.transitiveDirty = true
}

func (d *nodesTransitive[T]) MarkTransitiveDirty() {
	d.m.Lock()
	defer d.m.Unlock()

	d.transitiveDirty = true
}

func (d *nodesTransitive[T]) MarkTransitiveInvalid() {
	d.m.Lock()
	defer d.m.Unlock()

	d.transitiveNodes = nil
}

func (d *nodesTransitive[T]) Remove(dep *Node[T]) {
	d.m.Lock()
	defer d.m.Unlock()

	d.nodes.Remove(dep)
	d.transitiveNodes = nil
}

func (d *nodesTransitive[T]) Has(dep *Node[T]) bool {
	d.m.RLock()
	defer d.m.RUnlock()

	return d.nodes.Has(dep)
}

func (d *nodesTransitive[T]) TransitiveSet() *sets.Set[*Node[T], *Node[T]] {
	d.m.Lock()
	defer d.m.Unlock()

	if d.transitiveNodes == nil {
		d.transitiveNodes = d.computeTransitive(true)
	} else if d.transitiveDirty {
		d.transitiveNodes = d.computeTransitive(false)
	}
	d.transitiveDirty = false

	return d.transitiveNodes
}

func (d *nodesTransitive[T]) TransitiveValues() []T {
	return ads.Map(d.TransitiveSet().Slice(), func(t *Node[T]) T {
		return t.V
	})
}

func (d *nodesTransitive[T]) computeTransitive(full bool) *sets.Set[*Node[T], *Node[T]] {
	s := d.transitiveNodes
	if full {
		s = sets.NewIdentitySet[*Node[T]](0)
	}
	for _, dep := range d.nodes.Slice() {
		transitive := d.transitiveGetter(dep)

		if d.transitiveReverse {
			s.AddAll(transitive.TransitiveSet().Slice())
			s.Add(dep)
		} else {
			s.Add(dep)
			s.AddAll(transitive.TransitiveSet().Slice())
		}
	}
	return s
}

func newNodesTransitive[T any](transitiveGetter func(d *Node[T]) *nodesTransitive[T], transitiveReverse bool) *nodesTransitive[T] {
	return &nodesTransitive[T]{
		nodes:             sets.NewIdentitySet[*Node[T]](0),
		transitiveNodes:   sets.NewIdentitySet[*Node[T]](0),
		transitiveGetter:  transitiveGetter,
		transitiveReverse: transitiveReverse,
	}
}

type Node[T any] struct {
	V      T
	ID     string
	frozen bool
	m      sync.Mutex

	dependencies *nodesTransitive[T]
	dependees    *nodesTransitive[T]
}

func NewNode[T any](id string, v T) *Node[T] {
	return &Node[T]{
		ID: id,
		V:  v,
		dependencies: newNodesTransitive[T](func(d *Node[T]) *nodesTransitive[T] {
			return d.dependencies
		}, false),
		dependees: newNodesTransitive[T](func(d *Node[T]) *nodesTransitive[T] {
			return d.dependees
		}, true),
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
	if !d.dependencies.Has(dep) {
		if dep.dependencies.TransitiveSet().Has(d) {
			panic("cycle")
		}

		d.dependencies.Add(dep)

		for _, dependee := range d.dependees.TransitiveSet().Slice() {
			dependee.dependencies.MarkTransitiveDirty()
		}

		if !dep.dependees.Has(d) {
			dep.dependees.Add(d)

			for _, dep := range dep.dependencies.TransitiveSet().Slice() {
				dep.dependees.MarkTransitiveDirty()
			}
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

		for _, dependee := range d.dependees.TransitiveSet().Slice() {
			dependee.dependencies.MarkTransitiveInvalid()
		}
	}

	if dep.dependees.Has(d) {
		dep.dependees.Remove(dep)

		for _, dep := range dep.dependencies.TransitiveSet().Slice() {
			dep.dependees.MarkTransitiveInvalid()
		}
	}
}

func (d *Node[T]) IsFrozen() bool {
	d.m.Lock()
	defer d.m.Unlock()

	return d.frozen
}

// Freeze assumes the lock is already held
func (d *Node[T]) Freeze() {
	if d.frozen {
		return
	}

	for _, dep := range d.dependencies.nodes.Slice() {
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
	return d.dependencies.nodes.Slice()
}

func (d *Node[T]) Dependencies() []T {
	return d.toV(d.DependenciesNodes())
}

func (d *Node[T]) DependeesNodes() []*Node[T] {
	return d.dependees.nodes.Slice()
}

func (d *Node[T]) Dependees() []T {
	return d.toV(d.DependeesNodes())
}

func (d *Node[T]) DebugString() string {
	var sb strings.Builder
	fmt.Fprintf(&sb, "%v:\n", d.ID)
	deps := ads.Map(d.DependenciesNodes(), (*Node[T]).GetID)
	tdeps := ads.Map(d.dependencies.TransitiveSet().Slice(), (*Node[T]).GetID)
	fmt.Fprintf(&sb, "  deps: %v\n", deps)
	fmt.Fprintf(&sb, "  tdeps: %v\n", tdeps)

	depdees := ads.Map(d.DependeesNodes(), (*Node[T]).GetID)
	tdepdees := ads.Map(d.dependees.TransitiveSet().Slice(), (*Node[T]).GetID)
	fmt.Fprintf(&sb, "  depdees: %v\n", depdees)
	fmt.Fprintf(&sb, "  tdepdees: %v\n", tdepdees)

	return sb.String()
}
