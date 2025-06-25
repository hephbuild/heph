package htypes

func Must(err error) {
	if err != nil {
		panic(err)
	}
}

func Must2[T any](a T, err error) T {
	if err != nil {
		panic(err)
	}

	return a
}

func Must2Ok[T any](a T, ok bool) T {
	if !ok {
		panic("must be true")
	}

	return a
}
