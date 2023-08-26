package sets

import "sync"

type Set[K comparable, T any] struct {
	mu sync.RWMutex
	m  map[K]T
	a  []T
	f  func(T) K
}

func NewSet[K comparable, T any](f func(T) K, cap int) *Set[K, T] {
	t := &Set[K, T]{}
	t.m = make(map[K]T, cap)
	t.a = make([]T, 0, cap)
	t.f = f

	return t
}

func NewSetFrom[K comparable, T any](f func(T) K, vs []T) *Set[K, T] {
	s := NewSet[K, T](f, len(vs))
	s.AddAll(vs)

	return s
}

type StringSet = Set[string, string]

func NewStringSet(cap int) *StringSet {
	return NewIdentitySet[string](cap)
}

func NewIdentitySet[T comparable](cap int) *Set[T, T] {
	return NewSet(func(s T) T {
		return s
	}, cap)
}

func (ts *Set[K, T]) Add(t T) bool {
	ts.mu.Lock()
	defer ts.mu.Unlock()

	return ts.add(t)
}

func (ts *Set[K, T]) Has(t T) bool {
	ts.mu.RLock()
	defer ts.mu.RUnlock()

	return ts.has(t)
}

func (ts *Set[K, T]) GetKey(k K) T {
	ts.mu.RLock()
	defer ts.mu.RUnlock()

	return ts.m[k]
}

func (ts *Set[K, T]) has(t T) bool {
	k := ts.f(t)
	_, ok := ts.m[k]

	return ok
}

func (ts *Set[K, T]) add(t T) bool {
	k := ts.f(t)
	if ts.has(t) {
		return false
	}

	ts.m[k] = t
	ts.a = append(ts.a, t)

	return true
}

func (ts *Set[K, T]) AddAll(ats []T) {
	if len(ats) == 0 {
		return
	}

	ts.mu.Lock()
	defer ts.mu.Unlock()

	for _, t := range ats {
		ts.add(t)
	}
}

func (ts *Set[K, T]) Slice() []T {
	if ts == nil {
		return nil
	}

	return ts.a[:]
}

func (ts *Set[K, T]) Len() int {
	if ts == nil {
		return 0
	}

	return len(ts.a)
}

func (ts *Set[K, T]) Copy() *Set[K, T] {
	t := NewSet(ts.f, ts.Len())
	t.AddAll(ts.Slice())

	return t
}
