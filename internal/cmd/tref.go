package cmd

import (
	"context"
	"fmt"

	"github.com/hephbuild/heph/internal/engine"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
	"github.com/hephbuild/heph/tmatch"
	"github.com/spf13/cobra"
)

func parseTargetRef(s, cwd, root string) (*pluginv1.TargetRef, error) {
	cwp, err := tref.DirToPackage(cwd, root)
	if err != nil {
		return nil, err
	}

	ref, err := tref.ParseInPackage(s, cwp)
	if err != nil {
		return nil, fmt.Errorf("target: %w", err)
	}

	return ref, nil
}

func cobraArgs() cobra.PositionalArgs {
	return cobra.RangeArgs(1, 2)
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
			return nil, fmt.Errorf("package: %w", err)
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

func parseMatcherResolve(
	ctx context.Context,
	e *engine.Engine,
	rs *engine.RequestState,
	args []string,
	cwd,
	root string,
) (*pluginv1.TargetMatcher, *pluginv1.TargetRef, error) {
	matcher, err := parseMatcher(args, cwd, root)
	if err != nil {
		return nil, nil, err
	}

	if refm, ok := matcher.GetItem().(*pluginv1.TargetMatcher_Ref); ok {
		spec, err := e.GetSpec(ctx, rs, engine.SpecContainer{Ref: refm.Ref})
		if err != nil {
			return nil, nil, err
		}

		return matcher, spec.GetRef(), nil
	}

	return matcher, nil, nil
}
