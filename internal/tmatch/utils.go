package tmatch

import (
	"slices"

	"github.com/hephbuild/heph/internal/htypes"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
)

func All() *pluginv1.TargetMatcher {
	return PackagePrefix("")
}

func PackagePrefix(prefix string) *pluginv1.TargetMatcher {
	return pluginv1.TargetMatcher_builder{PackagePrefix: htypes.Ptr(prefix)}.Build()
}

func Package(pkg string) *pluginv1.TargetMatcher {
	return pluginv1.TargetMatcher_builder{Package: htypes.Ptr(pkg)}.Build()
}

func Ref(ref *pluginv1.TargetRef) *pluginv1.TargetMatcher {
	return pluginv1.TargetMatcher_builder{Ref: ref}.Build()
}

func Label(label string) *pluginv1.TargetMatcher {
	return pluginv1.TargetMatcher_builder{Label: htypes.Ptr(label)}.Build()
}

func And(ms ...*pluginv1.TargetMatcher) *pluginv1.TargetMatcher {
	ms = slices.DeleteFunc(ms, func(matcher *pluginv1.TargetMatcher) bool {
		return matcher == nil
	})

	switch len(ms) {
	case 0:
		return nil
	case 1:
		return ms[0]
	default:
		return pluginv1.TargetMatcher_builder{And: pluginv1.TargetMatchers_builder{Items: ms}.Build()}.Build()
	}
}

func Or(ms ...*pluginv1.TargetMatcher) *pluginv1.TargetMatcher {
	ms = slices.DeleteFunc(ms, func(matcher *pluginv1.TargetMatcher) bool {
		return matcher == nil
	})

	switch len(ms) {
	case 0:
		return nil
	case 1:
		return ms[0]
	default:
		return pluginv1.TargetMatcher_builder{Or: pluginv1.TargetMatchers_builder{Items: ms}.Build()}.Build()
	}
}

func Not(m *pluginv1.TargetMatcher) *pluginv1.TargetMatcher {
	return pluginv1.TargetMatcher_builder{Not: m}.Build()
}
