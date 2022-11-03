package utils

func Contains[T comparable](a []T, i T) bool {
	for _, ai := range a {
		if ai == i {
			return true
		}
	}

	return false
}
