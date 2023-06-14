package mds

import (
	"golang.org/x/exp/constraints"
	"sort"
)

func Keys[K constraints.Ordered, V any](m map[K]V) []K {
	ks := make([]K, 0, len(m))
	for k := range m {
		ks = append(ks, k)
	}
	sort.Slice(ks, func(i, j int) bool {
		return ks[i] < ks[j]
	})
	return ks
}

func Map[K, KO comparable, V, VO any](m map[K]V, f func(K, V) (KO, VO)) map[KO]VO {
	out := make(map[KO]VO, len(m))

	for k, v := range m {
		ko, vo := f(k, v)
		out[ko] = vo
	}

	return out
}
