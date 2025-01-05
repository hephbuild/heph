package hmaps

import (
	"cmp"
	"iter"
	"maps"
	"slices"
)

func Sorted[Map ~map[K]V, K cmp.Ordered, V any](m Map) iter.Seq2[K, V] {
	keys := slices.Sorted(maps.Keys(m))

	return func(yield func(K, V) bool) {
		for _, k := range keys {
			v := m[k]

			if !yield(k, v) {
				return
			}
		}
	}
}
