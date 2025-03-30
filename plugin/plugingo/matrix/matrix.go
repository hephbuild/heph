package matrix

import "maps"

type Variable struct {
	Key    string
	Values []string
}

func Generate(vars []Variable, skip []map[string]string) []map[string]string {
	if len(vars) == 0 {
		return nil
	}

	// Precompute total combinations
	total := 1
	for _, v := range vars {
		total *= len(v.Values)
	}

	shouldSkip := func(m map[string]string) bool {
		for _, skip := range skip {
			shouldSkip := true
			for k, v := range skip {
				if mv, ok := m[k]; !ok || mv != v {
					shouldSkip = false
					break
				}
			}

			if shouldSkip {
				return true
			}
		}

		return false
	}

	// Preallocate result slice
	result := make([]map[string]string, 0, total)
	current := make(map[string]string)

	var backtrack func(int)
	backtrack = func(depth int) {
		if depth == len(vars) {
			if shouldSkip(current) {
				return
			}

			// Create a copy to avoid pointer issues
			combinationCopy := maps.Clone(current)
			result = append(result, combinationCopy)
			return
		}

		v := vars[depth]
		for _, val := range v.Values {
			if val != "" {
				current[v.Key] = val
			}
			backtrack(depth + 1)
		}

		// Clean up current map for backtracking
		delete(current, v.Key)
	}

	backtrack(0)
	return result
}
