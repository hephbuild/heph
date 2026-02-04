package cmd

import (
	"fmt"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/spf13/cobra"
)

func init() {
	cmdArgs := parsePackageMatcherArgs{cmdName: "packages"}

	cmd := &cobra.Command{
		Use:               cmdArgs.Use(),
		Short:             "List the packages",
		Aliases:           []string{"pkgs"},
		Args:              cmdArgs.Args(),
		ValidArgsFunction: cmdArgs.ValidArgsFunction(),
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

			pkgMatcher, err := cmdArgs.Parse(args, cwd, root)
			if err != nil {
				return err
			}

			for pkg, err := range e.Packages(ctx, pkgMatcher) {
				if err != nil {
					return err
				}

				fmt.Println(pkg)
			}

			return nil
		},
	}

	inspectCmd.AddCommand(cmd)
}
