package utils

func Dedup[T any](as []T, id func(T) string) []T {
	value := make(map[string]struct{}, len(as))

	nas := as
	copied := false
	for i, a := range as {
		id := id(a)

		if _, ok := value[id]; ok {
			if !copied {
				nas = make([]T, i, len(as))
				copy(nas, as[:i])
				copied = true
			}
			continue
		}
		value[id] = struct{}{}
		if copied {
			nas = append(nas, a)
		}
	}

	return nas
}

func DedupKeepLast[T any](as []T, id func(T) string) []T {
	value := make(map[string]T, len(as))
	pos := make(map[string]int, len(as))

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

func MultiLess(compares ...func(i, j int) int) func(i, j int) bool {
	return func(i, j int) bool {
		for _, compare := range compares {
			r := compare(i, j)

			if r != 0 {
				return r < 0
			}
		}

		return false
	}
}
