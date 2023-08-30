package mds

import (
	"golang.org/x/exp/constraints"
	"golang.org/x/exp/slices"
)

func Keys[K constraints.Ordered, V any](m map[K]V) []K {
	ks := make([]K, 0, len(m))
	for k := range m {
		ks = append(ks, k)
	}
	slices.Sort(ks)
	return ks
}

func Values[K comparable, V any](m map[K]V) []V {
	vs := make([]V, 0, len(m))
	for _, v := range m {
		vs = append(vs, v)
	}
	return vs
}

func Map[K, KO comparable, V, VO any](m map[K]V, f func(K, V) (KO, VO)) map[KO]VO {
	out := make(map[KO]VO, len(m))

	for k, v := range m {
		ko, vo := f(k, v)
		out[ko] = vo
	}

	return out
}
