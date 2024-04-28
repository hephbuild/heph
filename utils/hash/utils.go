package hash

import "golang.org/x/exp/slices"

func HashArray[T any](h Hash, a []T, f func(T) string) {
	entries := make([]string, 0, len(a))
	for _, e := range a {
		entries = append(entries, f(e))
	}

	slices.Sort(entries)

	for _, entry := range entries {
		h.String(entry)
	}
}

func HashMap[K comparable, V any](h Hash, a map[K]V, f func(K, V) string) {
	entries := make([]string, 0, len(a))
	for k, v := range a {
		entries = append(entries, f(k, v))
	}

	slices.Sort(entries)

	for _, entry := range entries {
		h.String(entry)
	}
}

func HashString(s string) string {
	h := NewHash()
	h.String(s)
	return h.Sum()
}

func HashBytes(s []byte) string {
	h := NewHash()
	h.Write(s)
	return h.Sum()
}
