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
	"github.com/spf13/cobra"
)

func init() {
	var shell bool
	var force bool

	var runCmd = &cobra.Command{
		Use:     "run",
		Aliases: []string{"r"},
		Args:    cobra.ExactArgs(1),
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

			ref, err := parseTargetRef(args[0], cwd, root)
			if err != nil {
				return err
			}

			if plain || !isTerm() {
				return errors.New("must run in a term for now")
			}

			err = termui.NewInteractive(ctx, func(ctx context.Context, m termui.Model, send func(tea.Msg)) error {
				e, err := newEngine(ctx, root)
				if err != nil {
					return err
				}

				res, err := e.ResultFromRef(ctx, ref, []string{engine.AllOutputs}, engine.ResultOptions{
					InteractiveExec: func(ctx context.Context, iargs engine.InteractiveExecOptions) error {
						_, err := hbbtexec.Run(m.Exec, send, func(args hbbtexec.RunArgs) (struct{}, error) {
							if iargs.Pty {
								err := args.MakeRaw()
								if err != nil {
									return struct{}{}, err
								}
							}

							iargs.Run(ctx, engine.ExecOptions{
								Stdin: args.Stdin,
								// bbt has its output set to stderr, to prevent the CLI from outputting on stderr too,
								// we rely on os-provided stdout/stderr directly
								Stdout: os.Stdout,
								Stderr: os.Stderr,
							})

							return struct{}{}, nil
						})

						return err
					},
					Shell: shell,
					Force: force,
				}, engine.GlobalResolveCache)
				if err != nil {
					return err
				}
				defer res.Unlock(ctx)

				outputs := res.Artifacts

				// TODO how to render res natively without exec
				_, err = hbbtexec.Run(m.Exec, send, func(args hbbtexec.RunArgs) (struct{}, error) {
					for _, output := range outputs {
						fmt.Println(output.Name)
						fmt.Println("  group:    ", output.Group)
						fmt.Println("  uri:      ", output.Uri)
						fmt.Println("  type:     ", output.Type.String())
						fmt.Println("  encoding: ", output.Encoding.String())
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

	runCmd.Flags().BoolVarP(&shell, "shell", "", false, "shell into target")
	runCmd.Flags().BoolVarP(&force, "force", "", false, "force running")

	rootCmd.AddCommand(runCmd)
}
