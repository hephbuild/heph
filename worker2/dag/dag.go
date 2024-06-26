package dag

import (
	"fmt"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/sets"
	"iter"
	"strings"
	"sync"
	"sync/atomic"
)

type nodesTransitive[T any] struct {
	m                 sync.RWMutex
	nodes             *sets.Set[*Node[T], *Node[T]]
	transitiveNodes   *sets.Set[*Node[T], *Node[T]]
	transitiveDirty   bool
	transitiveGetter  func(d *Node[T]) *nodesTransitive[T]
	transitiveReverse bool
}

func (d *nodesTransitive[T]) Add(dep *Node[T]) bool {
	d.m.RLock()
	defer d.m.RUnlock()

	if d.nodes.Add(dep) {
		d.transitiveDirty = true
		return true
	}

	return false
}

func (d *nodesTransitive[T]) MarkTransitiveDirty() {
	d.m.RLock()
	defer d.m.RUnlock()

	d.transitiveDirty = true
}

func (d *nodesTransitive[T]) MarkTransitiveInvalid() {
	d.m.RLock()
	defer d.m.RUnlock()

	d.transitiveNodes = nil
}

func (d *nodesTransitive[T]) Remove(dep *Node[T]) {
	d.m.RLock()
	defer d.m.RUnlock()

	d.nodes.Remove(dep)
	d.transitiveNodes = nil
}

func (d *nodesTransitive[T]) Has(dep *Node[T]) bool {
	d.m.RLock()
	defer d.m.RUnlock()

	return d.nodes.Has(dep)
}

func (d *nodesTransitive[T]) Set() *sets.Set[*Node[T], *Node[T]] {
	d.m.RLock()
	defer d.m.RUnlock()

	return d.nodes
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

func (d *nodesTransitive[T]) TransitiveValues() iter.Seq2[int, T] {
	return func(yield func(int, T) bool) {
		for i, node := range d.TransitiveSet().Slice() {
			if !yield(i, node.V) {
				break
			}
		}
	}
}

func (d *nodesTransitive[T]) Values() iter.Seq2[int, T] {
	return func(yield func(int, T) bool) {
		for i, node := range d.Set().Slice() {
			if !yield(i, node.V) {
				break
			}
		}
	}
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

type Event interface {
	dagEvent()
}

type EventNewDep[T any] struct {
	Node *Node[T]
}

func (d EventNewDep[T]) dagEvent() {}

type Hook func(Event)

type Node[T any] struct {
	V      T
	ID     string
	frozen atomic.Bool
	m      sync.Mutex
	hooks  []Hook

	Dependencies *nodesTransitive[T]
	Dependees    *nodesTransitive[T]
}

func NewNode[T any](id string, v T) *Node[T] {
	return &Node[T]{
		ID: id,
		V:  v,
		Dependencies: newNodesTransitive[T](func(d *Node[T]) *nodesTransitive[T] {
			return d.Dependencies
		}, false),
		Dependees: newNodesTransitive[T](func(d *Node[T]) *nodesTransitive[T] {
			return d.Dependees
		}, true),
	}
}

func (d *Node[T]) GetID() string {
	return d.ID
}

func (d *Node[T]) AddHook(hook Hook) {
	d.m.Lock()
	defer d.m.Unlock()

	d.hooks = append(d.hooks, hook)
}

func (d *Node[T]) AddDependency(deps ...*Node[T]) {
	d.m.Lock()
	defer d.m.Unlock()

	if d.frozen.Load() {
		panic("add: frozen")
	}

	for _, dep := range deps {
		d.addDependency(dep)
	}
}

func (d *Node[T]) addDependency(dep *Node[T]) {
	if !d.Dependencies.Has(dep) {
		if dep.Dependencies.TransitiveSet().Has(d) {
			panic("cycle")
		}

		if d.Dependencies.Add(dep) {
			for _, dependee := range d.Dependees.TransitiveSet().Slice() {
				dependee.Dependencies.MarkTransitiveDirty()
			}

			if dep.Dependees.Add(d) {
				for _, dep := range dep.Dependencies.TransitiveSet().Slice() {
					dep.Dependees.MarkTransitiveDirty()
				}
			}

			for _, hook := range d.hooks {
				hook(EventNewDep[T]{Node: dep})
			}
		}
	}
}

func (d *Node[T]) RemoveDependency(dep *Node[T]) {
	d.m.Lock()
	defer d.m.Unlock()

	if d.IsFrozen() {
		panic("remove: deps is frozen")
	}

	if d.Dependencies.Has(dep) {
		d.Dependencies.Remove(dep)

		for _, dependee := range d.Dependees.TransitiveSet().Slice() {
			dependee.Dependencies.MarkTransitiveInvalid()
		}
	}

	if dep.Dependees.Has(d) {
		dep.Dependees.Remove(d)

		for _, dep := range dep.Dependencies.TransitiveSet().Slice() {
			dep.Dependees.MarkTransitiveInvalid()
		}
	}
}

func (d *Node[T]) IsFrozen() bool {
	return d.frozen.Load()
}

// Freeze will lock, and run valid across all dependencies, return false to prevent locking
func (d *Node[T]) Freeze(valid func(*Node[T]) bool) bool {
	d.m.Lock() // prevent any deps modification
	defer d.m.Unlock()

	if d.frozen.Load() {
		return true
	}

	for _, dep := range d.Dependencies.Set().Slice() {
		if !dep.IsFrozen() {
			panic(fmt.Sprintf("attempting to freeze '%v' while all deps aren't frozen, '%v' isnt", d.ID, dep.ID))
		}

		if !valid(dep) {
			return false
		}
	}

	d.frozen.Store(true)
	return true
}

func (d *Node[T]) DebugString() string {
	var sb strings.Builder
	fmt.Fprintf(&sb, "%v:\n", d.ID)
	deps := ads.Map(d.Dependencies.Set().Slice(), (*Node[T]).GetID)
	tdeps := ads.Map(d.Dependencies.TransitiveSet().Slice(), (*Node[T]).GetID)
	fmt.Fprintf(&sb, "  deps: %v\n", deps)
	fmt.Fprintf(&sb, "  tdeps: %v\n", tdeps)

	depdees := ads.Map(d.Dependees.Set().Slice(), (*Node[T]).GetID)
	tdepdees := ads.Map(d.Dependees.TransitiveSet().Slice(), (*Node[T]).GetID)
	fmt.Fprintf(&sb, "  depdees: %v\n", depdees)
	fmt.Fprintf(&sb, "  tdepdees: %v\n", tdepdees)

	return sb.String()
}
