package cmd

import (
	"context"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	"github.com/hephbuild/heph/internal/tmatch"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/spf13/cobra"
)

func init() {
	cmd := &cobra.Command{
		Use:  "validate [ <package-matcher> ]",
		Args: cobra.RangeArgs(0, 1),
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

			var pkgMatcher *pluginv1.TargetMatcher
			if len(args) == 1 {
				pkgMatcher, err = tmatch.ParsePackageMatcher(args[0], cwd, root)
				if err != nil {
					return err
				}
			} else {
				pkgMatcher = tmatch.All()
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
