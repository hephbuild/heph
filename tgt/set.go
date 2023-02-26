package tgt

import (
	"github.com/puzpuzpuz/xsync/v2"
	"hash/maphash"
	"heph/targetspec"
	"sort"
	"sync"
)

type TargetsSet = Store[*Target]

func NewTargetsSet(cap int) *TargetsSet {
	return NewSet[*Target](cap)
}

type StoreID interface {
	GetUID() uint64
	targetspec.TargetBase
}

type Store[T StoreID] struct {
	m    *xsync.MapOf[uint64, T]
	mfqn *xsync.MapOf[string, T]

	mu     sync.Mutex
	all    []T
	public *Store[T]
	specs  []targetspec.TargetSpec
	fqns   []string
	allh   int
}

func NewSet[T StoreID](cap int) *Store[T] {
	s := &Store[T]{}

	s.m = xsync.NewIntegerMapOfPresized[uint64, T](cap)
	s.mfqn = xsync.NewTypedMapOfPresized[string, T](func(seed maphash.Seed, fqn string) uint64 {
		var h maphash.Hash
		h.SetSeed(seed)
		h.WriteString(fqn)
		return h.Sum64()
	}, cap)

	return s
}

func (ts *Store[T]) Add(t T) {
	ts.m.Store(t.GetUID(), t)
	ts.mfqn.Store(t.GetFQN(), t)
}

func (ts *Store[T]) FQNs() []string {
	ts.genSlices()
	return ts.fqns
}

func (ts *Store[T]) Specs() []targetspec.TargetSpec {
	ts.genSlices()
	return ts.specs
}

func (ts *Store[T]) Public() *Store[T] {
	ts.genSlices()
	return ts.public
}

func (ts *Store[T]) Slice() []T {
	ts.genSlices()
	return ts.all
}

func (ts *Store[T]) genSlices() {
	if ts.allh == ts.Len() {
		return
	}

	ts.mu.Lock()
	defer ts.mu.Unlock()

	l := ts.Len()

	if ts.allh == l {
		return
	}

	all := make([]T, 0, l)
	fqns := make([]string, 0, l)
	publics := NewSet[T](l / 2)
	specs := make([]targetspec.TargetSpec, 0, l)
	ts.m.Range(func(key uint64, target T) bool {
		all = append(all, target)
		fqns = append(fqns, target.GetFQN())
		if !target.IsPrivate() {
			publics.Add(target)
		}
		specs = append(specs, target.GetSpec())
		return true
	})

	sort.Slice(all, func(i, j int) bool {
		return all[i].GetFQN() < all[j].GetFQN()
	})
	sort.Slice(specs, func(i, j int) bool {
		return specs[i].GetFQN() < specs[j].GetFQN()
	})
	sort.Slice(fqns, func(i, j int) bool {
		return fqns[i] < fqns[j]
	})

	ts.all = all
	ts.fqns = fqns
	ts.public = publics
	ts.specs = specs
	ts.allh = l
}

func (ts *Store[T]) Sorted() []T {
	return ts.Slice()
}

func (ts *Store[T]) Find(t targetspec.TargetFQN) T {
	if t, ok := t.(StoreID); ok {
		v, _ := ts.m.Load(t.GetUID())
		return v
	}

	v, _ := ts.mfqn.Load(t.GetFQN())
	return v
}

func (ts *Store[T]) FindBy(fqn string) T {
	v, _ := ts.mfqn.Load(fqn)
	return v
}

func (ts *Store[T]) Len() int {
	return ts.m.Size()
}
