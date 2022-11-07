package utils

func Map[T, O any](a []T, f func(T) O) []O {
	out := make([]O, len(a))

	for i, e := range a {
		out[i] = f(e)
	}

	return out
}
