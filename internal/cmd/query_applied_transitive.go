package cmd

import (
	"context"
	"fmt"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	"github.com/spf13/cobra"
	"google.golang.org/protobuf/encoding/protojson"
)

func init() {
	var link bool
	cmdArgs := parseRefArgs{cmdName: "applied-transitive"}

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

			err = newTermui(ctx, func(ctx context.Context, execFunc func(f hbbtexec.ExecFunc) error) error {
				e, err := newEngine(ctx, root)
				if err != nil {
					return err
				}

				rs, cleanRs := e.NewRequestState()
				defer cleanRs()

				var def *engine.TargetDef
				if link {
					res, err := e.Link(ctx, rs, engine.DefContainer{Ref: ref})
					if err != nil {
						return err
					}

					def = res.TargetDef
				} else {
					res, err := e.GetDef(ctx, rs, engine.DefContainer{Ref: ref})
					if err != nil {
						return err
					}

					def = res
				}

				// TODO how to render res natively without exec
				err = execFunc(func(args hbbtexec.RunArgs) error {
					fmt.Println(protojson.Format(def.AppliedTransitive))

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

	cmd.Flags().BoolVar(&link, "link", false, "Link target")

	queryCmd.AddCommand(cmd)
}
