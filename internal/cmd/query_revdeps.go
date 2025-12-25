package cmd

import (
	"context"
	"fmt"

	"github.com/hephbuild/heph/internal/tmatch"
	"github.com/hephbuild/heph/lib/tref"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	"github.com/spf13/cobra"
)

func init() {
	var scope string

	cmdArgs := parseRefArgs{cmdName: "revdeps"}

	cmd := &cobra.Command{
		Use:               cmdArgs.Use(),
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

			ref, err := cmdArgs.Parse(args[0], cwd, root)
			if err != nil {
				return err
			}

			scopeMatcher := tmatch.All()
			if scope != "" {
				scopeMatcher, err = tmatch.ParsePackageMatcher(scope, cwd, root)
				if err != nil {
					return err
				}
			}

			err = newTermui(ctx, func(ctx context.Context, execFunc func(f hbbtexec.ExecFunc) error) error {
				e, err := newEngine(ctx, root)
				if err != nil {
					return err
				}

				rs, cleanRs := e.NewRequestState()
				defer cleanRs()

				desc, err := e.DAG(ctx, rs, ref, engine.DAGTypeDescendants, scopeMatcher)
				if err != nil {
					return err
				}

				// TODO how to render res natively without exec
				err = execFunc(func(args hbbtexec.RunArgs) error {
					for _, ref := range desc.GetVertices() {
						fmt.Println(tref.Format(ref))
					}

					return nil
				})
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

	cmd.Flags().StringVar(&scope, "scope", "", "Filter universe of targets")

	queryCmd.AddCommand(cmd)
}
