package hmaps

func Concat[M map[K]V, K comparable, V any](ms ...M) M {
	var l int
	for _, m := range ms {
		l += len(m)
	}
	out := make(M, l)
	for _, m := range ms {
		for k, v := range m {
			out[k] = v
		}
	}

	return out
}
