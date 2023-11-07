package specs

func IsLabelMatcher(m Matcher) bool {
	if _, ok := m.(labelNode); ok {
		return true
	}

	if _, ok := m.(labelRegexNode); ok {
		return true
	}

	return false
}

func IsAddrMatcher(m Matcher) bool {
	if _, ok := m.(TargetAddr); ok {
		return true
	}

	if _, ok := m.(addrRegexNode); ok {
		return true
	}

	return false
}

func AndNodeFactory[T Matcher](ms ...T) Matcher {
	switch len(ms) {
	case 0:
		return NoneMatcher
	case 1:
		return ms[0]
	default:
		or := mAndNode{nodes: make([]Matcher, 0, len(ms))}

		for _, m := range ms {
			if any(m) == NoneMatcher {
				return NoneMatcher
			}

			if any(m) == AllMatcher {
				continue
			}

			switch n := any(m).(type) {
			case andNode:
				or.nodes = append(or.nodes, n.left, n.right)
			case mAndNode:
				or.nodes = append(or.nodes, n.nodes...)
			default:
				or.nodes = append(or.nodes, m)
			}
		}

		switch len(or.nodes) {
		case 0:
			return NoneMatcher
		case 1:
			return or.nodes[0]
		case 2:
			return andNode{or.nodes[0], or.nodes[1]}
		}

		return or
	}
}

func OrNodeFactory[T Matcher](ms ...T) Matcher {
	switch len(ms) {
	case 0:
		return NoneMatcher
	case 1:
		return ms[0]
	default:
		or := mOrNode{nodes: make([]Matcher, 0, len(ms))}

		for _, m := range ms {
			if any(m) == NoneMatcher {
				continue
			}

			if any(m) == AllMatcher {
				return AllMatcher
			}

			switch n := any(m).(type) {
			case orNode:
				or.nodes = append(or.nodes, n.left, n.right)
			case mOrNode:
				or.nodes = append(or.nodes, n.nodes...)
			default:
				or.nodes = append(or.nodes, m)
			}
		}

		switch len(or.nodes) {
		case 0:
			return NoneMatcher
		case 1:
			return or.nodes[0]
		case 2:
			return orNode{or.nodes[0], or.nodes[1]}
		}

		return or
	}
}
