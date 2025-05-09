package hslices

func UniqBy[T any, U comparable, Slice ~[]T](collection Slice, iteratee func(item T) U) Slice {
	result := make(Slice, 0, len(collection))
	seen := make(map[U]struct{}, len(collection))

	for i := range collection {
		key := iteratee(collection[i])

		if _, ok := seen[key]; ok {
			continue
		}

		seen[key] = struct{}{}
		result = append(result, collection[i])
	}

	return result
}
