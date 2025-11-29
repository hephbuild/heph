package cmd

import (
	"bufio"
	"context"
	"fmt"
	"os"
	"strings"

	"github.com/hephbuild/heph/internal/tmatch"
	"github.com/hephbuild/heph/lib/tref"

	"github.com/hephbuild/heph/internal/engine"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
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

func parseMatcherCobraArgs() cobra.PositionalArgs {
	return cobra.RangeArgs(1, 2)
}

func parseMatcher(args []string, cwd, root string) (*pluginv1.TargetMatcher, error) {
	switch len(args) {
	case 1:
		// TODO: complicated expression with `-e` flag

		if args[0] == "-" {
			return parseMatcherFromStdin(cwd, root)
		}

		ref, err := parseTargetRef(args[0], cwd, root)
		if err != nil {
			return nil, err
		}

		return tmatch.Ref(ref), nil
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
			matchers = append(matchers, tmatch.Label(label))
		}

		return tmatch.And(matchers...), nil
	default:
		panic("unhandled")
	}
}

func parseMatcherFromStdin(cwd, root string) (*pluginv1.TargetMatcher, error) {
	var matchers []*pluginv1.TargetMatcher

	sc := bufio.NewScanner(os.Stdin)
	for sc.Scan() {
		s := strings.TrimSpace(sc.Text())
		if s == "" {
			continue
		}

		ref, err := parseTargetRef(s, cwd, root)
		if err != nil {
			return nil, err
		}

		matchers = append(matchers, tmatch.Ref(ref))
	}

	return tmatch.Or(matchers...), nil
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

	if ref := matcher.GetRef(); ref != nil {
		spec, err := e.GetSpec(ctx, rs, engine.SpecContainer{Ref: ref})
		if err != nil {
			return nil, nil, err
		}

		return matcher, spec.GetRef(), nil
	}

	return matcher, nil, nil
}
