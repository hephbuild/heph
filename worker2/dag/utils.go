package dag

import (
	"github.com/hephbuild/heph/utils/xsync"
	"golang.org/x/exp/maps"
)

type Pool[T comparable] struct {
	xsync.Pool[map[T]struct{}]
}

func (p *Pool[T]) Get() (map[T]struct{}, func()) {
	m := p.Pool.Get()
	return m, func() {
		p.Put(m)
	}
}

func (p *Pool[T]) Put(m map[T]struct{}) {
	maps.Clear(m)
	p.Pool.Put(m)
}

func NewPool[T comparable]() Pool[T] {
	return Pool[T]{
		xsync.Pool[map[T]struct{}]{New: func() map[T]struct{} {
			return map[T]struct{}{}
		}},
	}
}

type DeepDoOptions[T any] struct {
	Func            func(*Node[T])
	MemoizerFactory func() (map[*Node[T]]struct{}, func())
}

func DeepDoMany[T any](opts DeepDoOptions[T], as []*Node[T]) {
	switch len(as) {
	case 0:
		return
	case 1:
		DeepDo(opts, as[0])
		return
	}

	m, clean := opts.MemoizerFactory()
	defer clean()

	for _, a := range as {
		if a.IsFrozen() {
			deepDoPrecomputedMemoized(a, opts.Func, m)
		} else {
			deepDoRecursiveInner(a, opts.Func, m)
		}
	}
}

func DeepDo[T any](opts DeepDoOptions[T], a *Node[T]) {
	if a.IsFrozen() {
		deepDoPrecomputed(a, opts.Func)
	} else {
		m, clean := opts.MemoizerFactory()
		defer clean()
		deepDoRecursiveInner(a, opts.Func, m)
	}
}

// This approach can prove costly when the deps are being recomputed as deps are being added
func deepDoPrecomputed[T any](a *Node[T], f func(*Node[T])) {
	f(a)
	for _, dep := range a.Dependencies.OrderedTransitiveSet().Slice() {
		f(dep)
	}
}

// This approach can prove costly when the deps are being recomputed as deps are being added
func deepDoPrecomputedMemoized[T any](a *Node[T], f func(*Node[T]), m map[*Node[T]]struct{}) {
	if _, ok := m[a]; ok {
		return
	}
	m[a] = struct{}{}

	f(a)

	for _, dep := range a.Dependencies.OrderedTransitiveSet().Slice() {
		if _, ok := m[a]; ok {
			continue
		}
		m[a] = struct{}{}

		f(dep)
	}
}

func deepDoInner[T any](a *Node[T], f func(*Node[T]), m map[*Node[T]]struct{}) bool {
	if _, ok := m[a]; ok {
		return false
	}
	m[a] = struct{}{}

	f(a)

	return true
}

func deepDoRecursiveInner[T any](a *Node[T], f func(*Node[T]), m map[*Node[T]]struct{}) {
	if !deepDoInner(a, f, m) {
		return
	}

	if a.IsFrozen() {
		for _, dep := range a.Dependencies.OrderedTransitiveSet().Slice() {
			deepDoInner(dep, f, m)
		}
	} else {
		for _, dep := range a.Dependencies.Set().Slice() {
			deepDoRecursiveInner(dep, f, m)
		}
	}
}
