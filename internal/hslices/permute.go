package hslices

import (
	"iter"
	"slices"
)

func Permute[T any](arr []T) iter.Seq[[]T] {
	return permute(arr, 0, len(arr)-1)
}

func permute[T any](arr []T, l int, r int) iter.Seq[[]T] {
	if len(arr) == 0 {
		return func(yield func([]T) bool) {
			yield(arr)
		}
	}

	return func(yield func([]T) bool) {
		if l == r {
			yield(slices.Clone(arr))
		} else {
			for i := l; i <= r; i++ {
				arr[l], arr[i] = arr[i], arr[l]
				for range permute[T](arr, l+1, r) {
					if !yield(slices.Clone(arr)) {
						return
					}
				}
				arr[l], arr[i] = arr[i], arr[l] // backtrack
			}
		}
	}
}
