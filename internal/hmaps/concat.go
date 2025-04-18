package hmaps

func Concat[M map[K]V, K comparable, V any](ms...M) M {
	out := M{}
	for _, m := range ms {
		for k, v := range m {
			out[k] = v
		}
	}

	return out
}
