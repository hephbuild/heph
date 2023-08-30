package ads

import (
	"golang.org/x/exp/slices"
	"sort"
)

type Comparer[T any] func(i, j T) int

func sortCompare[T any](ai, aj T, comparers []Comparer[T]) int {
	for _, compare := range comparers {
		r := compare(ai, aj)

		if r != 0 {
			return r
		}
	}

	return 0
}

func SortFunc[S ~[]E, E any](x S, less func(a, b E) bool) {
	slices.SortFunc(x, func(a, b E) int {
		if less(a, b) {
			return -1
		}

		return +1
	})
}

// Sort sorts by passing a copy of the data around
func Sort[T any](a []T, comparers ...Comparer[T]) {
	slices.SortFunc(a, func(a, b T) int {
		return sortCompare(a, b, comparers)
	})
}

type sorterp[T any] struct {
	a         []T
	comparers []Comparer[*T]
}

func (s *sorterp[T]) Len() int {
	return len(s.a)
}

func (s *sorterp[T]) Less(i, j int) bool {
	ai := &s.a[i]
	aj := &s.a[j]

	return sortCompare(ai, aj, s.comparers) < 0
}

func (s *sorterp[T]) Swap(i, j int) {
	s.a[i], s.a[j] = s.a[j], s.a[i]
}

// SortP sorts by passing a pointer of the data around
func SortP[T any](a []T, comparers ...Comparer[*T]) {
	s := &sorterp[T]{a, comparers}
	sort.Sort(s)
}
