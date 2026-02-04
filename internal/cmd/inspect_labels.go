package cmd

import (
	"context"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	"github.com/spf13/cobra"
)

func init() {
	cmdArgs := parsePackageMatcherArgs{cmdName: "labels"}

	cmd := &cobra.Command{
		Use:               cmdArgs.Use(),
		Short:             "List the labels in the universe of targets",
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

			err = newTermui(ctx, func(ctx context.Context, execFunc func(f hbbtexec.ExecFunc) error) error {
				e, err := newEngine(ctx, root)
				if err != nil {
					return err
				}

				rs, cleanRs := e.NewRequestState()
				defer cleanRs()

				queue, flushResults := render(ctx, execFunc)
				defer flushResults()

				for label, err := range e.Labels(ctx, rs, matcher) {
					if err != nil {
						return err
					}

					queue(label)
				}

				return nil
			})
			if err != nil {
				return err
			}

			return nil
		},
	}

	inspectCmd.AddCommand(cmd)
}
