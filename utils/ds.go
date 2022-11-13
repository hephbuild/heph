package utils

func Map[T, O any](a []T, f func(T) O) []O {
	out := make([]O, len(a))

	for i, e := range a {
		out[i] = f(e)
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
