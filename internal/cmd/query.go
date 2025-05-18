package cmd

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	"github.com/hephbuild/heph/plugin/tref"
	"github.com/spf13/cobra"
)

// heph r :lint          run //some:lint (assuming pwd = some)
// heph r :lint .        run //some:lint (assuming pwd = some)
// heph r //other:lint   run //other:lint
// heph r lint //...     run all lint targets

// heph r lint ./...     run all test target under cwd
// heph r lint ./foo/... run all test target under foo
// heph r lint .

// heph r //:lint

// heph r -e 'lint && //some/**/*'
// heph r -e 'test || (k8s-validate && !k8s-validate-prod)'

var queryCmd = func() *cobra.Command {
	return &cobra.Command{
		Use:     "query",
		Aliases: []string{"q"},
		Args:    cobra.RangeArgs(1, 2),
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
