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

func flattenAndNodeInto(m Matcher, ms *[]Matcher, isLast bool) bool {
	if any(m) == NoneMatcher {
		*ms = []Matcher{NoneMatcher}
		return true
	}

	if any(m) == AllMatcher {
		if isLast && len(*ms) == 0 {
			*ms = []Matcher{AllMatcher}
		}
		return false
	}

	switch m := m.(type) {
	case andNode:
		*ms = ads.GrowExtra(*ms, len(m.nodes))
		for i, cm := range m.nodes {
			if flattenAndNodeInto(cm, ms, isLast && i == len(m.nodes)-1) {
				return true
			}
		}
	default:
		*ms = append(*ms, m)
	}

	return false
}

func NotNodeFactory(m Matcher) Matcher {
	switch m {
	case AllMatcher:
		return NoneMatcher
	case NoneMatcher:
		return AllMatcher
	default:
		return notNode{m}
	}
}

func AndNodeFactory[T Matcher](ms ...T) Matcher {
	switch len(ms) {
	case 0:
		return NoneMatcher
	case 1:
		return ms[0]
	default:
		or := andNode{nodes: make([]Matcher, 0, len(ms))}

		for i, m := range ms {
			if flattenAndNodeInto(m, &or.nodes, i == len(ms)-1) {
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

func flattenOrNodeInto(m Matcher, ms *[]Matcher, isLast bool) bool {
	if any(m) == AllMatcher {
		*ms = []Matcher{AllMatcher}
		return true
	}

	if any(m) == NoneMatcher {
		if len(*ms) == 0 {
			*ms = []Matcher{NoneMatcher}
		}
		return false
	}

	switch m := m.(type) {
	case orNode:
		*ms = ads.GrowExtra(*ms, len(m.nodes))
		for i, cm := range m.nodes {
			if flattenOrNodeInto(cm, ms, isLast && i == len(m.nodes)-1) {
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

		for i, m := range ms {
			if flattenOrNodeInto(m, &or.nodes, i == len(ms)-1) {
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
