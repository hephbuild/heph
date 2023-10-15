package specs

func starIntersect(s1, s2 string, i, j int) bool {
	if i == len(s1) && j == len(s2) {
		return true
	}

	if i == len(s1) {
		if s2[j:] == "/**" || s2[j:] == "**" {
			return true
		}
		return false
	}

	if j == len(s2) {
		if s1[i:] == "/**" || s1[i:] == "**" {
			return true
		}
		return false
	}

	if s1[i] == '*' {
		if i+1 < len(s1) && s1[i+1] == '*' {
			if starIntersect(s1, s2, i+2, j) || starIntersect(s1, s2, i+2, j+1) || starIntersect(s1, s2, i, j+1) {
				return true
			}

			if i+2 < len(s1) && s1[i+2] == '/' {
				if starIntersect(s1, s2, i+3, j) || starIntersect(s1, s2, i+3, j+1) || starIntersect(s1, s2, i, j+1) {
					return true
				}
			}
		}
		if s2[j] == '/' {
			return starIntersect(s1, s2, i+1, j)
		}
		return starIntersect(s1, s2, i+1, j) || starIntersect(s1, s2, i+1, j+1) || starIntersect(s1, s2, i, j+1)
	}

	if s2[j] == '*' {
		if j+1 < len(s2) && s2[j+1] == '*' {
			if starIntersect(s1, s2, i+1, j) || starIntersect(s1, s2, i+1, j+2) || starIntersect(s1, s2, i, j+2) {
				return true
			}

			if j+2 < len(s2) && s2[j+2] == '/' {
				if starIntersect(s1, s2, i+1, j) || starIntersect(s1, s2, i+1, j+3) || starIntersect(s1, s2, i, j+3) {
					return true
				}
			}
		}
		if s1[i] == '/' {
			return starIntersect(s1, s2, i, j+1)
		}
		return starIntersect(s1, s2, i+1, j) || starIntersect(s1, s2, i+1, j+1) || starIntersect(s1, s2, i, j+1)
	}

	return s1[i] == s2[j] && starIntersect(s1, s2, i+1, j+1)
}

func starMatch(s1, s2 string, i, j int) bool {
	if i == len(s1) && j == len(s2) {
		return true
	}

	if i == len(s1) || j == len(s2) {
		return false
	}

	if s2[j] == '*' {
		if j+1 < len(s2) && s2[j+1] == '*' {
			if starMatch(s1, s2, i+1, j) || starMatch(s1, s2, i+1, j+2) || starMatch(s1, s2, i, j+2) {
				return true
			}

			if j+2 < len(s2) && s2[j+2] == '/' {
				if starMatch(s1, s2, i+1, j) || starMatch(s1, s2, i+1, j+3) || starMatch(s1, s2, i, j+3) {
					return true
				}
			}
		}
		if s1[i] == '/' {
			return starMatch(s1, s2, i, j+1)
		}
		return starMatch(s1, s2, i+1, j) || starMatch(s1, s2, i+1, j+1) || starMatch(s1, s2, i, j+1)
	}

	return s1[i] == s2[j] && starMatch(s1, s2, i+1, j+1)
}
