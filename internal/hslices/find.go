package hslices

func Find[T any](vs []T, match func(T) bool) (T, bool) {
	for _, v := range vs {
		if match(v) {
			return v, true
		}
	}

	var zero T
	return zero, false
}
