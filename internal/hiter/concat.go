package hiter

import "iter"

func To1[K, V any](s iter.Seq2[K, V]) iter.Seq[V] {
	return func(yield func(V) bool) {
		for _, v := range s {
			if !yield(v) {
				return
			}
		}
	}
}

func Concat[V any](ss ...iter.Seq[V]) iter.Seq[V] {
	return func(yield func(V) bool) {
		for _, s := range ss {
			for v := range s {
				if !yield(v) {
					return
				}
			}
		}
	}
}

func Concat2[K, V any](ss ...iter.Seq2[K, V]) iter.Seq2[K, V] {
	return func(yield func(K, V) bool) {
		for _, s := range ss {
			for k, v := range s {
				if !yield(k, v) {
					return
				}
			}
		}
	}
}

func Index[V any](s iter.Seq[V]) iter.Seq2[int, V] {
	return func(yield func(int, V) bool) {
		var i int
		for v := range s {
			if !yield(i, v) {
				return
			}
			i++
		}
	}
}
