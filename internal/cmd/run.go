//nolint:forbidigo
package cmd

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"os"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/hephbuild/hephv2/internal/engine"
	"github.com/hephbuild/hephv2/internal/hbbt/hbbtexec"
	"github.com/hephbuild/hephv2/internal/hfs"
	"github.com/hephbuild/hephv2/internal/termui"
	"github.com/hephbuild/hephv2/plugin/pluginbuildfile"
	"github.com/hephbuild/hephv2/plugin/pluginexec"
	"github.com/spf13/cobra"
)

func init() {
	var shell bool
	var force bool

	var runCmd = &cobra.Command{
		Use:  "run",
		Args: cobra.ExactArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx := cmd.Context()

			if plain || !isTerm() {
				return errors.New("must run in a term for now")
			}

			err := termui.NewInteractive(ctx, func(ctx context.Context, m termui.Model, send func(tea.Msg)) error {
				root, err := engine.Root()
				if err != nil {
					return err
				}

				e, err := engine.New(ctx, root, engine.Config{})
				if err != nil {
					return err
				}

				_, err = e.RegisterProvider(ctx, pluginbuildfile.New(hfs.NewOS(root)))
				if err != nil {
					return err
				}

				providers := []*pluginexec.Plugin{
					pluginexec.New(),
					pluginexec.NewSh(),
					pluginexec.NewBash(),
					pluginexec.NewInteractiveBash(),
				}

				for _, p := range providers {
					_, err = e.RegisterDriver(ctx, p, func(mux *http.ServeMux) {
						path, h := p.PipesHandler()
						mux.Handle(path, h)
					})
					if err != nil {
						return err
					}
				}

				ch := e.Result(ctx, args[0], args[1], []string{engine.AllOutputs}, engine.ResultOptions{
					InteractiveExec: func(iargs engine.InteractiveExecOptions) error {
						_, err = hbbtexec.Run(m.Exec, send, func(args hbbtexec.RunArgs) (struct{}, error) {
							if iargs.Pty {
								err = args.MakeRaw()
								if err != nil {
									return struct{}{}, err
								}
							}

							iargs.Run(engine.ExecOptions{
								Stdin:  args.Stdin,
								Stdout: os.Stdout,
								Stderr: os.Stderr,
							})

							return struct{}{}, nil
						})

						return err
					},
					Shell: shell,
					Force: force,
				})

				res := <-ch

				if res.Err != nil {
					return res.Err
				}

				outputs := res.Outputs

				// TODO how to render res natively without exec
				_, err = hbbtexec.Run(m.Exec, send, func(args hbbtexec.RunArgs) (*engine.ExecuteResult, error) {
					for _, output := range outputs {
						fmt.Println(output.Name)
						fmt.Println("  group:    ", output.Group)
						fmt.Println("  uri:      ", output.Uri)
						fmt.Println("  type:     ", output.Type.String())
						fmt.Println("  encoding: ", output.Encoding.String())
					}

					return res, nil
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
