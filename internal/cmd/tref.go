package cmd

import (
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
	"github.com/hephbuild/heph/tmatch"
)

func parseTargetRef(s, cwd, root string) (*pluginv1.TargetRef, error) {
	cwp, err := tref.DirToPackage(cwd, root)
	if err != nil {
		return nil, err
	}

	return tref.ParseInPackage(s, cwp)
}

func parseMatcher(args []string, cwd, root string) (*pluginv1.TargetMatcher, error) {
	switch len(args) {
	case 1:
		// TODO: complicated expression with `-e` flag
		// TODO: targets list from stdin

		ref, err := parseTargetRef(args[0], cwd, root)
		if err != nil {
			return nil, err
		}

		return &pluginv1.TargetMatcher{Item: &pluginv1.TargetMatcher_Ref{Ref: ref}}, nil
	case 2:
		pkgMatcher, err := tmatch.ParsePackageMatcher(args[1], cwd, root)
		if err != nil {
			return nil, err
		}

		matchers := []*pluginv1.TargetMatcher{
			pkgMatcher,
		}
		label := args[0]
		if label != "all" {
			matchers = append(matchers, &pluginv1.TargetMatcher{Item: &pluginv1.TargetMatcher_Label{Label: label}})
		}

		return &pluginv1.TargetMatcher{Item: &pluginv1.TargetMatcher_And{And: &pluginv1.TargetMatchers{Items: matchers}}}, nil
	default:
		panic("unhandled")
	}
}
