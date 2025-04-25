package hmaps

func Keyed[T comparable](vs []T) map[T]struct{} {
	m := map[T]struct{}{}
	for _, v := range vs {
		m[v] = struct{}{}
	}
	return m
}
