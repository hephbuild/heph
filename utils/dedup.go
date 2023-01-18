package utils

import "heph/utils/sets"

func Dedup[T any](as []T, id func(T) string) []T {
	s := sets.NewSetFrom[string, T](id, as)

	return s.Slice()
}

func DedupKeepLast[T any](as []T, id func(T) string) []T {
	value := make(map[string]T, len(as))
	pos := make(map[string]int, len(as))

	for i, a := range as {
		a := a
		id := id(a)

		value[id] = a
		pos[id] = i
	}

	nas := make([]T, 0, len(as))
	for i, a := range as {
		id := id(a)

		pi := pos[id]
		if pi == i {
			nas = append(nas, value[id])
		}
	}

	return nas
}

func MultiLess(compares ...func(i, j int) int) func(i, j int) bool {
	return func(i, j int) bool {
		for _, compare := range compares {
			r := compare(i, j)

			if r != 0 {
				return r < 0
			}
		}

		return false
	}
}
