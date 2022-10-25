package engine

import (
	"heph/targetspec"
	"strings"
)

type TargetMatcher func(*Target) bool

type TargetMatchers []TargetMatcher

// AnyMatch
// Deprecated, use OrMatcher instead
func (ms TargetMatchers) AnyMatch(target *Target) bool {
	return OrMatcher(ms...)(target)
}

func NotMatcher(m TargetMatcher) TargetMatcher {
	return func(target *Target) bool {
		return !m(target)
	}
}

func AndMatcher(ms ...TargetMatcher) TargetMatcher {
	if len(ms) == 1 {
		return ms[0]
	}

	return func(target *Target) bool {
		for _, m := range ms {
			if !m(target) {
				return false
			}
		}

		return true
	}
}

func OrMatcher(ms ...TargetMatcher) TargetMatcher {
	if len(ms) == 1 {
		return ms[0]
	}

	return func(target *Target) bool {
		for _, m := range ms {
			if m(target) {
				return true
			}
		}

		return false
	}
}

func ParseTargetSelector(pkg, s string) TargetMatcher {
	isAllDeep := false
	isAll := false
	if strings.HasSuffix(s, "...") {
		isAllDeep = true
		s = strings.TrimSuffix(s, "...")
		s = strings.TrimSuffix(s, "/")
	} else if strings.HasSuffix(s, ".") {
		isAll = true
		s = strings.TrimSuffix(s, ".")
		s = strings.TrimSuffix(s, "/")
	}

	tp, err := targetspec.TargetParse(pkg, s)
	if err == nil {
		if isAllDeep {
			return func(target *Target) bool {
				pkg := target.Package.FullName
				return pkg == tp.Package || strings.HasPrefix(pkg, tp.Package+"/")
			}
		} else if isAll {
			return func(target *Target) bool {
				pkg := target.Package.FullName
				return pkg == tp.Package
			}
		}

		return func(target *Target) bool {
			return target.FQN == tp.Full()
		}
	}

	return func(target *Target) bool {
		return target.HasAnyLabel([]string{s})
	}
}
