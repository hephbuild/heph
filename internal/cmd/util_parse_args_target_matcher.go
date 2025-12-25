package cmd

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"os"
	"strings"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/tmatch"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/spf13/cobra"
)

type ValidArgsFunction = func(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective)

type parseTargetMatcherArgs struct {
	cmdName string
}

func (r parseTargetMatcherArgs) Use() string {
	return fmt.Sprintf("%v {<label> <package-matcher> | <target-addr>}", r.cmdName)
}

func (r parseTargetMatcherArgs) hint() string {
	return fmt.Sprintf("must be `%[1]v <label> <package-matcher>` or `%[1]v <target-addr>`", r.cmdName)
}

func (r parseTargetMatcherArgs) Args() cobra.PositionalArgs {
	return func(cmd *cobra.Command, args []string) error {
		switch len(args) {
		case 1, 2:
			return nil
		default:
			return errors.New(r.hint())
		}
	}
}

func (r parseTargetMatcherArgs) ValidArgsFunction() ValidArgsFunction {
	return nil // TODO
}

func (r parseTargetMatcherArgs) Parse(args []string, cwd, root string) (*pluginv1.TargetMatcher, error) {
	switch len(args) {
	case 1:
		// TODO: complicated expression with `-e` flag

		if args[0] == "-" {
			return r.parseMatcherFromStdin(cwd, root)
		}

		ref, err := parseTargetRef(args[0], cwd, root)
		if err != nil {
			return nil, fmt.Errorf("%w: %v", err, r.hint())
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

func (r parseTargetMatcherArgs) parseMatcherFromStdin(cwd, root string) (*pluginv1.TargetMatcher, error) {
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

	if err := sc.Err(); err != nil {
		return nil, err
	}

	return tmatch.Or(matchers...), nil
}

func (r parseTargetMatcherArgs) ParseResolve(
	ctx context.Context,
	e *engine.Engine,
	rs *engine.RequestState,
	args []string,
	cwd,
	root string,
) (*pluginv1.TargetMatcher, *pluginv1.TargetRef, error) {
	matcher, err := r.Parse(args, cwd, root)
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
