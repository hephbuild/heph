package hiter

import "iter"

func Len[T any](s iter.Seq[T]) int {
	var i int
	for range s {
		i++
	}
	return i
}
