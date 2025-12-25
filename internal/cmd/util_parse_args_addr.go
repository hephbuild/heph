package cmd

import (
	"errors"
	"fmt"

	"github.com/hephbuild/heph/lib/tref"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/spf13/cobra"
)

type parseRefArgs struct {
	cmdName string
}

func (r parseRefArgs) Use() string {
	return fmt.Sprintf("%v <target-addr>", r.cmdName)
}

func (r parseRefArgs) hint() string {
	return fmt.Sprintf("must be `%[1]v <target-addr>`", r.cmdName)
}

func (r parseRefArgs) Args() cobra.PositionalArgs {
	return func(cmd *cobra.Command, args []string) error {
		switch len(args) {
		case 1:
			return nil
		default:
			return errors.New(r.hint())
		}
	}
}

func (r parseRefArgs) ValidArgsFunction() ValidArgsFunction {
	return nil // TODO
}

func (r parseRefArgs) Parse(s, cwd, root string) (*pluginv1.TargetRef, error) {
	cwp, err := tref.DirToPackage(cwd, root)
	if err != nil {
		return nil, err
	}

	ref, err := tref.ParseInPackage(s, cwp)
	if err != nil {
		return nil, fmt.Errorf("target: %w: %v", err, r.hint())
	}

	return ref, nil
}
