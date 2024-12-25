package hmaps

func GetSet[K comparable, V any](m map[K]V, k K) (func() (V, bool), func(V)) {
	return func() (V, bool) {
			v, ok := m[k]
			return v, ok
		}, func(v V) {
			m[k] = v
		}
}
