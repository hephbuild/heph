//nolint:forbidigo
package cmd

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/signal"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	"github.com/hephbuild/heph/internal/termui"
	"github.com/hephbuild/heph/plugin/tref"
	"github.com/spf13/cobra"
)

func init() {
	cmd := &cobra.Command{
		Use:  "deps",
		Args: cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx := cmd.Context()

			if plain || !isTerm() {
				return errors.New("must run in a term for now")
			}

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

			ref, err := parseTargetRef(args[0], cwd, root)
			if err != nil {
				return err
			}

			err = termui.NewInteractive(ctx, func(ctx context.Context, m termui.Model, send func(tea.Msg)) error {
				e, err := newEngine(ctx, root)
				if err != nil {
					return err
				}

				def, err := e.LightLink(ctx, engine.DefContainer{Ref: ref})
				if err != nil {
					return err
				}

				// TODO how to render res natively without exec
				_, err = hbbtexec.Run(m.Exec, send, func(args hbbtexec.RunArgs) (struct{}, error) {
					for _, dep := range def.Inputs {
						fmt.Println(tref.Format(dep.GetRef()))
					}

					return struct{}{}, nil
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
