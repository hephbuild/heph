package ads

import (
	"fmt"
	"golang.org/x/exp/slices"
)

func Map[T, O any](a []T, f func(T) O) []O {
	if a == nil {
		return nil
	}

	out := make([]O, len(a))

	for i, e := range a {
		out[i] = f(e)
	}

	return out
}

func MapE[T, O any](a []T, f func(T) (O, error)) ([]O, error) {
	if a == nil {
		return nil, nil
	}

	out := make([]O, len(a))

	var err error
	for i, e := range a {
		out[i], err = f(e)
		if err != nil {
			return out, err
		}
	}

	return out, nil
}

func Contains[T comparable](a []T, e T) bool {
	for _, ae := range a {
		if ae == e {
			return true
		}
	}

	return false
}

func ContainsAny[T comparable](a []T, e []T) bool {
	for _, ee := range e {
		if Contains(a, ee) {
			return true
		}
	}

	return false
}

func ContainsAll[T comparable](a []T, e []T) bool {
	for _, ee := range e {
		if !Contains(a, ee) {
			return false
		}
	}

	return true
}

func Filter[S ~[]E, E any](a S, f func(E) bool) S {
	o := a
	alloc := false

	for i, e := range a {
		keep := f(e)

		if !alloc {
			if !keep {
				o = make(S, i)
				alloc = true
				if i > 0 {
					copy(o, a[:i])
				}
			}
		} else {
			if keep {
				o = append(o, e)
			}
		}
	}

	return o
}

func MapFlat[T comparable](a []T, f func(T) []T) []T {
	o, _ := MapFlatE(a, func(t T) ([]T, error) {
		return f(t), nil
	})
	return o
}

func MapFlatE[T comparable](a []T, f func(T) ([]T, error)) ([]T, error) {
	o := a
	alloc := false

	for i, e := range a {
		fr, err := f(e)
		if err != nil {
			return o, err
		}

		if !alloc {
			if len(fr) == 1 && fr[0] == e {
				continue
			}

			o = make([]T, i, len(o))
			alloc = true
			if i > 0 {
				copy(o, a[:i])
			}
		}

		o = append(o, fr...)
	}

	return o, nil
}

func Find[T any](a []T, f func(T) bool) (T, bool) {
	for _, e := range a {
		if f(e) {
			return e, true
		}
	}

	var empty T
	return empty, false
}

func FindIndex[T any](a []T, f func(T) bool) int {
	for i, e := range a {
		if f(e) {
			return i
		}
	}

	return -1
}

func Copy[S ~[]E, E any](a S) S {
	if a == nil {
		return nil
	}

	var empty S
	return append(empty, a...)
}

func Chunk[T any](slice []T, chunkSize int) [][]T {
	if chunkSize == 1 {
		return [][]T{slice}
	}

	chunks := make([][]T, 0, chunkSize)
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

func Remove[T comparable](slice []T, e T) []T {
	i := FindIndex(slice, func(t T) bool {
		return e == t
	})
	if i < 0 {
		return slice
	}

	return RemoveIndex(slice, i)
}

func RemoveIndex[T any](slice []T, s int) []T {
	return append(slice[:s], slice[s+1:]...)
}

type Group[T any, K comparable] struct {
	Key   K
	Items []T
}

func OrderedGroupBy[T any, K comparable](a []T, keyer func(T) K, less func(i, j T) bool) []Group[T, K] {
	m := make(map[K]int)
	ga := make([]Group[T, K], 0)

	for _, o := range a {
		o := o
		k := keyer(o)
		if i, ok := m[k]; ok {
			ga[i].Items = append(ga[i].Items, o)
		} else {
			ga = append(ga, Group[T, K]{
				Key:   k,
				Items: []T{o},
			})
			m[k] = len(ga) - 1
		}
	}

	for _, g := range ga {
		SortFunc(g.Items, func(a, b T) bool {
			return less(a, b)
		})
	}

	SortFunc(ga, func(a, b Group[T, K]) bool {
		return less(a.Items[0], b.Items[0])
	})

	return ga
}

func GrowExtra[S ~[]E, E any](slice S, extraCap int) S {
	return Grow(slice, len(slice)+extraCap)
}

func Grow[S ~[]E, E any](slice S, newCap int) S {
	if cap(slice) >= newCap {
		return slice
	}

	if newCap < len(slice) {
		panic(fmt.Sprintf("Grow: newCap is smaller than existing slice len, slice len: %v, newCap: %v", len(slice), newCap))
	}

	newSlice := make(S, len(slice), newCap)
	copy(newSlice, slice)

	return newSlice
}

func Reduce[T any, O any](a []T, f func(O, T) O, initial O) O {
	out, _ := ReduceE[T, O](a, func(o O, t T) (O, error) {
		return f(o, t), nil
	}, initial)
	return out
}

func ReduceE[T any, O any](a []T, f func(O, T) (O, error), initial O) (O, error) {
	out := initial

	var err error
	for _, v := range a {
		out, err = f(out, v)
		if err != nil {
			return out, err
		}
	}

	return out, nil
}

func Last[T any](a []T) T {
	return a[len(a)-1]
}

func LastP[T any](a []T) *T {
	return &a[len(a)-1]
}

func Some[T any](a []T, f func(e T) bool) bool {
	for _, e := range a {
		if f(e) {
			return true
		}
	}

	return false
}

func All[T any](a []T, f func(e T) bool) bool {
	for _, e := range a {
		if !f(e) {
			return false
		}
	}

	return true
}

func Reverse[S ~[]E, E any](a S) S {
	r := Copy(a)
	slices.Reverse(r)
	return r
}
