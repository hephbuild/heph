package hslices

func GrowLen[S ~[]E, E any](s S, n int) S {
	if n < 0 {
		panic("cannot be negative")
	}
	if n -= cap(s) - len(s); n > 0 {
		// This expression allocates only once (see test).
		s = append(s[:cap(s)], make([]E, n)...)
	}
	return s
}
