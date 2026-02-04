package cmd

import (
	"context"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	"github.com/spf13/cobra"
)

func init() {
	cmdArgs := parsePackageMatcherArgs{cmdName: "validate"}

	cmd := &cobra.Command{
		Use:               cmdArgs.Use(),
		Short:             "Ensures the build graph is valid",
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

			pkgMatcher, err := cmdArgs.Parse(args, cwd, root)
			if err != nil {
				return err
			}

			err = newTermui(ctx, func(ctx context.Context, execFunc func(f hbbtexec.ExecFunc) error) error {
				e, err := newEngine(ctx, root)
				if err != nil {
					return err
				}

				rs, cleanRs := e.NewRequestState()
				defer cleanRs()

				err = e.Validate(ctx, rs, pkgMatcher)
				if err != nil {
					return err
				}

				return nil
			})
			if err != nil {
				return err
			}

			return nil
		},
	}

	rootCmd.AddCommand(cmd)
}
