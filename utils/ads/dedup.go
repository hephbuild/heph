package ads

func DedupAppendIdentity[T comparable](as []T, vs ...T) []T {
	return DedupAppend(as, func(t T) T {
		return t
	}, vs...)
}

func DedupAppend[T any, K comparable](as []T, id func(T) K, vs ...T) []T {
	if len(vs) == 0 {
		return as
	}

	appender := DedupAppender(as, id, len(vs))

	for _, v := range vs {
		as = appender(as, v)
	}

	return as
}

func DedupAppenderIdentity[T comparable](as []T, cap int) func([]T, T) []T {
	return DedupAppender(as, func(t T) T {
		return t
	}, cap)
}

func DedupAppender[T any, K comparable](as []T, id func(T) K, cap int) func([]T, T) []T {
	value := make(map[K]struct{}, len(as)+cap)
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

func DedupIdentity[T comparable](as []T) []T {
	return Dedup(as, func(t T) T {
		return t
	})
}

func Dedup[T any, K comparable](as []T, id func(T) K) []T {
	value := make(map[K]struct{}, len(as))

	return Filter(as, func(e T) bool {
		id := id(e)

		if _, ok := value[id]; ok {
			return false
		}

		value[id] = struct{}{}
		return true
	})
}
