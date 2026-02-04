package cmd

import (
	"context"

	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/internal/tmatch"
	"github.com/hephbuild/heph/lib/tref"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	"github.com/spf13/cobra"
)

func init() {
	var ignore []string

	cmdArgs := parseTargetMatcherArgs{cmdName: "clean"}

	cmd := &cobra.Command{
		Use:               cmdArgs.Use(),
		Short:             "Clear the cache for targets matching the given pattern",
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

			matcher, err := cmdArgs.Parse(args, cwd, root)
			if err != nil {
				return err
			}

			for _, s := range ignore {
				ignoreMatcher, err := tmatch.ParsePackageMatcher(s, cwd, root)
				if err != nil {
					return err
				}

				matcher = tmatch.And(matcher, tmatch.Not(ignoreMatcher))
			}

			err = newTermui(ctx, func(ctx context.Context, execFunc func(f hbbtexec.ExecFunc) error) error {
				e, err := newEngine(ctx, root)
				if err != nil {
					return err
				}

				rs, cleanRs := e.NewRequestState()
				defer cleanRs()

				var count uint
				for ref, err := range e.Query(ctx, rs, matcher) {
					if err != nil {
						return err
					}

					if err := ctx.Err(); err != nil {
						return err
					}

					err := e.ClearCacheLocally(ctx, ref, "")
					if err != nil {
						hlog.From(ctx).Error("clearing cache", "ref", tref.Format(ref), "err", err)
					}

					count++
				}

				hlog.From(ctx).Info("cleaned", "count", count)

				return nil
			})
			if err != nil {
				return err
			}

			return nil
		},
	}

	cmd.Flags().StringArrayVar(&ignore, "ignore", nil, "Filter universe of targets")

	rootCmd.AddCommand(cmd)
}
