package cmd

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	"github.com/hephbuild/heph/plugin/tref"
	"github.com/spf13/cobra"
	"os"
	"os/signal"
)

// heph r :deploy          run //some:deploy
// heph r deploy           run all deploy targets         <= should this be possible ? kinda risky
// heph r deploy //...     run all deploy targets
// heph r deploy ./...     run all test target under cwd
// heph r deploy ./foo/... run all test target under foo

var queryCmd = func() *cobra.Command {
	return &cobra.Command{
		Use:     "query",
		Aliases: []string{"q"},
		Args:    cobra.RangeArgs(1, 2),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx := cmd.Context()

			ctx, stop := signal.NotifyContext(ctx, os.Interrupt)
			defer stop()

			cwd, err := engine.Cwd()
			if err != nil {
				return err
			}

			root, err := engine.Root()
			if err != nil {
				return err
			}

			matcher, err := parseMatcher(args, cwd, root)
			if err != nil {
				return err
			}

			err = newTermui(ctx, func(ctx context.Context, execFunc func(f hbbtexec.ExecFunc) error) error {
				e, err := newEngine(ctx, root)
				if err != nil {
					return err
				}

				refs := e.Query(ctx, matcher)
				for ref, err := range refs {
					if err != nil {
						return err
					}

					if err := ctx.Err(); err != nil {
						return err
					}

					// TODO: batch write
					err := execFunc(func(args hbbtexec.RunArgs) error {
						fmt.Println(tref.Format(ref))

						return nil
					})
					if err != nil {
						return err
					}
				}

				return nil
			})
			if err != nil {
				return err
			}

			return nil
		},
	}
}()

func init() {
	rootCmd.AddCommand(queryCmd)
}
