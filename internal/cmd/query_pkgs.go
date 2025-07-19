package cmd

import (
	"fmt"

	"github.com/hephbuild/heph/internal/tmatch"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/spf13/cobra"
)

func init() {
	cmd := &cobra.Command{
		Use:     "packages",
		Aliases: []string{"pkgs"},
		Args:    cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx := cmd.Context()

			ctx, stop := newSignalNotifyContext(ctx)
			defer stop()

			cwd, err := engine.Cwd()
			if err != nil {
				return err
			}

			root, err := engine.Root()
			if err != nil {
				return err
			}

			e, err := newEngine(ctx, root)
			if err != nil {
				return err
			}

			pkgMatcher, err := tmatch.ParsePackageMatcher(args[0], cwd, root)
			if err != nil {
				return err
			}

			for pkg, err := range e.Packages(ctx, pkgMatcher) {
				if err != nil {
					return err
				}

				if err := ctx.Err(); err != nil {
					return err
				}

				fmt.Println(pkg)
			}

			return nil
		},
	}

	queryCmd.AddCommand(cmd)
}
