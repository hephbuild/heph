package utils

func DedupAppend[T any](as []T, id func(T) string, vs ...T) []T {
	if len(vs) == 0 {
		return as
	}

	appender := DedupAppender(as, id, len(vs))

	for _, v := range vs {
		as = appender(as, v)
	}

	return as
}

func DedupAppender[T any](as []T, id func(T) string, cap int) func([]T, T) []T {
	value := make(map[string]struct{}, len(as)+cap)
	for _, a := range as {
		id := id(a)

		value[id] = struct{}{}
	}

	return func(as []T, v T) []T {
		id := id(v)
		if _, ok := value[id]; ok {
			return as
		}
		value[id] = struct{}{}

		return append(as, v)
	}
}

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
