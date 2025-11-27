package hmaps

import (
	"cmp"
	"iter"
	"maps"
	"slices"
)

func Sorted[Map ~map[K]V, K cmp.Ordered, V any](m Map) iter.Seq2[K, V] {
	return func(yield func(K, V) bool) {
		switch len(m) {
		case 0:
			return
		case 1:
			for k, v := range m {
				if !yield(k, v) {
					return
				}
			}

			return
		}

		keys := slices.Sorted(maps.Keys(m))

		for _, k := range keys {
			v := m[k]

			if !yield(k, v) {
				return
			}
		}
	}
}
