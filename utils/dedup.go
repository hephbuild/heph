package utils

func DedupKeepLast[T any](as []T, id func(T) string) []T {
	value := map[string]T{}
	pos := map[string]int{}

	for i, a := range as {
		a := a
		id := id(a)

		value[id] = a
		pos[id] = i
	}

	nas := make([]T, 0, len(as))
	for i, a := range as {
		id := id(a)

		pi := pos[id]
		if pi == i {
			nas = append(nas, value[id])
		}
	}

	return nas
}
