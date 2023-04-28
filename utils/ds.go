package utils

import "sort"

func Map[T, O any](a []T, f func(T) O) []O {
	out := make([]O, len(a))

	for i, e := range a {
		out[i] = f(e)
	}

	return out
}

func MapMap[K, KO comparable, V, VO any](m map[K]V, f func(K, V) (KO, VO)) map[KO]VO {
	out := make(map[KO]VO, len(m))

	for k, v := range m {
		ko, vo := f(k, v)
		out[ko] = vo
	}

	return out
}

func Contains[T comparable](a []T, e T) bool {
	for _, ae := range a {
		if ae == e {
			return true
		}
	}

	return false
}

func Filter[T any](a []T, f func(T) bool) []T {
	o := make([]T, 0)

	for _, e := range a {
		if f(e) {
			o = append(o, e)
		}
	}

	return o
}

func CopyArray[T any](a []T) []T {
	if a == nil {
		return nil
	}

	var empty []T
	return append(empty, a...)
}

func Keys[K string, V any](m map[K]V) []K {
	ks := make([]K, 0, len(m))
	for k := range m {
		ks = append(ks, k)
	}
	sort.Slice(ks, func(i, j int) bool {
		return ks[i] < ks[j]
	})
	return ks
}

func ChunkSlice[T any](slice []T, chunkSize int) [][]T {
	var chunks [][]T
	for {
		if len(slice) == 0 {
			break
		}

		if len(slice) < chunkSize {
			chunkSize = len(slice)
		}

		chunks = append(chunks, slice[0:chunkSize])
		slice = slice[chunkSize:]
	}

	return chunks
}

func RemoveSlice[T any](slice []T, s int) []T {
	return append(slice[:s], slice[s+1:]...)
}
