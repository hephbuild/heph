package tmatch

import (
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

func Walk(matcher *pluginv1.TargetMatcher, f func(*pluginv1.TargetMatcher) bool) {
	_ = walk(matcher, f)
}

func walk(matcher *pluginv1.TargetMatcher, f func(*pluginv1.TargetMatcher) bool) bool {
	switch matcher.WhichItem() {
	case pluginv1.TargetMatcher_Or_case:
		for _, m := range matcher.GetAnd().GetItems() {
			if !walk(m, f) {
				return false
			}
		}
	case pluginv1.TargetMatcher_And_case:
		for _, m := range matcher.GetAnd().GetItems() {
			if !walk(m, f) {
				return false
			}
		}
	case pluginv1.TargetMatcher_Not_case:
		if !walk(matcher.GetNot(), f) {
			return false
		}
	default:
		return f(matcher)
	}

	return true
}
