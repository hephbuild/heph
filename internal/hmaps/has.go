package hmaps

func Has[K comparable, V any](vs map[K]V, match func(K, V) bool) bool {
	for k, v := range vs {
		if match(k, v) {
			return true
		}
	}

	return false
}
