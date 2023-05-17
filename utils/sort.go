package utils

import "sort"

type Comparer[T any] func(i, j T) int

type sorter[T any] struct {
	a         []T
	comparers []Comparer[T]
}

func (s *sorter[T]) Len() int {
	return len(s.a)
}

func (s *sorter[T]) Less(i, j int) bool {
	ai := s.a[i]
	aj := s.a[j]

	for _, compare := range s.comparers {
		r := compare(ai, aj)

		if r != 0 {
			return r < 0
		}
	}

	return false
}

func (s *sorter[T]) Swap(i, j int) {
	s.a[i], s.a[j] = s.a[j], s.a[i]
}

func Sort[T any](a []T, comparers ...Comparer[T]) {
	s := &sorter[T]{a, comparers}
	sort.Sort(s)
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

	for _, compare := range s.comparers {
		r := compare(ai, aj)

		if r != 0 {
			return r < 0
		}
	}

	return false
}

func (s *sorterp[T]) Swap(i, j int) {
	s.a[i], s.a[j] = s.a[j], s.a[i]
}

func SortP[T any](a []T, comparers ...Comparer[*T]) {
	s := &sorterp[T]{a, comparers}
	sort.Sort(s)
}
