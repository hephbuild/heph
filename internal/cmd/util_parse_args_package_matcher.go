package cmd

import (
	"errors"
	"fmt"

	"github.com/hephbuild/heph/internal/tmatch"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/spf13/cobra"
)

type parsePackageMatcherArgs struct {
	cmdName   string
	mandatory bool
}

func (r parsePackageMatcherArgs) Use() string {
	if r.mandatory {
		return fmt.Sprintf("%v <package-matcher>", r.cmdName)
	}

	return fmt.Sprintf("%v [ <package-matcher> ]", r.cmdName)
}

func (r parsePackageMatcherArgs) hint() string {
	if r.mandatory {
		return fmt.Sprintf("must be `%[1]v <package-matcher>`", r.cmdName)
	}

	return fmt.Sprintf("must be `%[1]v [ <package-matcher> ]`", r.cmdName)
}

func (r parsePackageMatcherArgs) Args() cobra.PositionalArgs {
	return func(cmd *cobra.Command, args []string) error {
		if r.mandatory {
			if len(args) == 1 {
				return nil
			}
		} else {
			if len(args) == 0 || len(args) == 1 {
				return nil
			}
		}

		return errors.New(r.hint())
	}
}

func (r parsePackageMatcherArgs) ValidArgsFunction() ValidArgsFunction {
	return nil // TODO
}

func (r parsePackageMatcherArgs) Parse(args []string, cwd, root string) (*pluginv1.TargetMatcher, error) {
	if len(args) == 0 {
		return tmatch.All(), nil
	}

	return tmatch.ParsePackageMatcher(args[0], cwd, root)
}
