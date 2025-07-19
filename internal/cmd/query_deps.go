package cmd

import (
	"context"
	"fmt"

	"github.com/hephbuild/heph/lib/tref"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	"github.com/spf13/cobra"
)

func init() {
	cmd := &cobra.Command{
		Use:  "deps",
		Args: cobra.ExactArgs(1),
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

			ref, err := parseTargetRef(args[0], cwd, root)
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

				def, err := e.Link(ctx, rs, engine.DefContainer{Ref: ref})
				if err != nil {
					return err
				}

				// TODO how to render res natively without exec
				err = execFunc(func(args hbbtexec.RunArgs) error {
					for _, dep := range def.Inputs {
						fmt.Println(tref.Format(dep.GetRef()))
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

	queryCmd.AddCommand(cmd)
}
