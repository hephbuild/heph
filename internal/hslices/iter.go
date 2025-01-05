package hslices

import "iter"

func AllFunc[I, O any](a []I, f func(I) O) iter.Seq2[int, O] {
	return func(yield func(int, O) bool) {
		for i, v := range a {
			if !yield(i, f(v)) {
				return
			}
		}
	}
}
