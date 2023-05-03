package ads

func Map[T, O any](a []T, f func(T) O) []O {
	out := make([]O, len(a))

	for i, e := range a {
		out[i] = f(e)
	}

	return out
}

func MapFilter[T, O any](a []T, f func(T) (O, bool)) []O {
	out := make([]O, 0, len(a))

	for _, e := range a {
		v, ok := f(e)
		if ok {
			out = append(out, v)
		}
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

func Copy[T any](a []T) []T {
	if a == nil {
		return nil
	}

	var empty []T
	return append(empty, a...)
}

func Chunk[T any](slice []T, chunkSize int) [][]T {
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
