package specs

import "github.com/hephbuild/heph/utils/ads"

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

func flattenAndNodeInto(m Matcher, ms *[]Matcher) bool {
	if any(m) == NoneMatcher {
		*ms = []Matcher{NoneMatcher}
		return true
	}

	if any(m) == AllMatcher {
		return false
	}

	switch m := m.(type) {
	case andNode:
		*ms = ads.GrowExtra(*ms, len(m.nodes))
		for _, cm := range m.nodes {
			if flattenAndNodeInto(cm, ms) {
				return true
			}
		}
	default:
		*ms = append(*ms, m)
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
		or := andNode{nodes: make([]Matcher, 0, len(ms))}

		for _, m := range ms {
			if flattenAndNodeInto(m, &or.nodes) {
				break
			}
		}

		switch len(or.nodes) {
		case 0:
			return NoneMatcher
		case 1:
			return or.nodes[0]
		}

		return or
	}
}

func flattenOrNodeInto(m Matcher, ms *[]Matcher) bool {
	if any(m) == AllMatcher {
		*ms = []Matcher{AllMatcher}
		return true
	}

	if any(m) == NoneMatcher {
		return false
	}

	switch m := m.(type) {
	case orNode:
		*ms = ads.GrowExtra(*ms, len(m.nodes))
		for _, cm := range m.nodes {
			if flattenAndNodeInto(cm, ms) {
				return true
			}
		}
	default:
		*ms = append(*ms, m)
	}

	return false
}

func OrNodeFactory[T Matcher](ms ...T) Matcher {
	switch len(ms) {
	case 0:
		return NoneMatcher
	case 1:
		return ms[0]
	default:
		or := orNode{nodes: make([]Matcher, 0, len(ms))}

		for _, m := range ms {
			if flattenOrNodeInto(m, &or.nodes) {
				break
			}
		}

		switch len(or.nodes) {
		case 0:
			return NoneMatcher
		case 1:
			return or.nodes[0]
		}

		return or
	}
}
