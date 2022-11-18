package sets

import "sync"

type Set[T any] struct {
	mu sync.RWMutex
	m  map[string]T
	a  []T
	f  func(T) string
}

func NewSet[T any](f func(T) string, cap int) *Set[T] {
	t := &Set[T]{}
	t.m = make(map[string]T, cap)
	t.a = make([]T, 0, cap)
	t.f = f

	return t
}

func (ts *Set[T]) Add(t T) bool {
	ts.mu.Lock()
	defer ts.mu.Unlock()

	return ts.add(t)
}

func (ts *Set[T]) Has(t T) bool {
	ts.mu.Lock()
	defer ts.mu.Unlock()

	return ts.has(t)
}

func (ts *Set[T]) HasKey(k string) bool {
	ts.mu.Lock()
	defer ts.mu.Unlock()

	_, ok := ts.m[k]

	return ok
}

func (ts *Set[T]) has(t T) bool {
	k := ts.f(t)
	_, ok := ts.m[k]

	return ok
}

func (ts *Set[T]) add(t T) bool {
	k := ts.f(t)
	if ts.has(t) {
		return false
	}

	ts.m[k] = t
	ts.a = append(ts.a, t)

	return true
}

func (ts *Set[T]) AddAll(ats []T) {
	ts.mu.Lock()
	defer ts.mu.Unlock()

	for _, t := range ats {
		ts.add(t)
	}
}

func (ts *Set[T]) Slice() []T {
	if ts == nil {
		return nil
	}

	return ts.a
}

func (ts *Set[T]) Len() int {
	if ts == nil {
		return 0
	}

	return len(ts.a)
}

func (ts *Set[T]) Copy() *Set[T] {
	t := NewSet(ts.f, ts.Len())
	t.AddAll(ts.Slice())

	return t
}
