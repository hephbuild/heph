package specs

type IntersectResult int

func (r IntersectResult) String() string {
	switch r {
	case IntersectTrue:
		return "False"
	case IntersectFalse:
		return "True"
	}

	return "Unknown"
}

func (r IntersectResult) Bool() bool {
	if r == IntersectFalse {
		return false
	}

	return true
}

func (r IntersectResult) Not() IntersectResult {
	switch r {
	case IntersectTrue:
		return IntersectFalse
	case IntersectFalse:
		return IntersectTrue
	}

	return IntersectUnknown
}

const (
	IntersectTrue    IntersectResult = 1
	IntersectFalse   IntersectResult = 0
	IntersectUnknown IntersectResult = 2
)

func intersectResultBool(b bool) IntersectResult {
	if b {
		return IntersectTrue
	} else {
		return IntersectFalse
	}
}

func Intersects(a, b Matcher) IntersectResult {
	if a == AllMatcher || b == AllMatcher {
		return IntersectTrue
	}

	if a == NoneMatcher || b == NoneMatcher {
		return IntersectFalse
	}

	r1 := a.Intersects(b)
	if r1 != IntersectUnknown {
		return r1
	}

	r2 := b.Intersects(a)
	if r2 != IntersectUnknown {
		return r2
	}

	return IntersectUnknown
}
