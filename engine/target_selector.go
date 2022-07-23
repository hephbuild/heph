package engine

import (
	"heph/utils"
	"strings"
)

type TargetMatcher func(*Target) bool

type TargetMatchers []TargetMatcher

func (ms TargetMatchers) AnyMatch(target *Target) bool {
	for _, m := range ms {
		if m(target) {
			return true
		}
	}

	return false
}

func ParseTargetSelector(pkg, s string) TargetMatcher {
	isAll := false
	if strings.HasSuffix(s, "...") {
		isAll = true
		s = strings.TrimSuffix(s, "...")
		s = strings.TrimSuffix(s, "/")
	}

	tp, err := utils.TargetParse(pkg, s)
	if err == nil {
		if isAll {
			return func(target *Target) bool {
				pkg := target.Package.FullName
				return pkg == tp.Package || strings.HasPrefix(pkg, tp.Package+"/")
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
