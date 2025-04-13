//nolint:forbidigo
package cmd

import (
	"context"
	"errors"
	"fmt"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/termui"
	"github.com/hephbuild/heph/plugin/pluginbuildfile"
	"github.com/hephbuild/heph/plugin/pluginexec"
	"github.com/hephbuild/heph/plugin/pluginfs"
	"github.com/hephbuild/heph/plugin/plugingo"
	"github.com/hephbuild/heph/plugin/tref"
	"github.com/spf13/cobra"
	"net/http"
	"os"
	"os/signal"
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

			ref, err := tref.Parse(args[0])
			if err != nil {
				return err
			}

			if plain || !isTerm() {
				return errors.New("must run in a term for now")
			}

			c := make(chan os.Signal, 1)
			signal.Notify(c, os.Interrupt)
			defer signal.Stop(c)

			ctx, cancel := context.WithCancel(ctx)
			defer cancel()

			go func() {
				<-c
				cancel()
			}()

			err = termui.NewInteractive(ctx, func(ctx context.Context, m termui.Model, send func(tea.Msg)) error {
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

				_, err = e.RegisterProvider(ctx, plugingo.New())
				if err != nil {
					return err
				}

				_, err = e.RegisterProvider(ctx, pluginfs.NewProvider())
				if err != nil {
					return err
				}
				//_, err = e.RegisterDriver(ctx, pluginfs.NewDriver())
				//if err != nil {
				//	return err
				//}

				drivers := []*pluginexec.Plugin{
					pluginexec.New(),
					pluginexec.NewSh(),
					pluginexec.NewBash(),
					pluginexec.NewInteractiveBash(),
				}

				for _, p := range drivers {
					_, err = e.RegisterDriver(ctx, p, func(mux *http.ServeMux) {
						path, h := p.PipesHandler()
						mux.Handle(path, h)
					})
					if err != nil {
						return err
					}
				}

				ch := e.ResultFromRef(ctx, ref, []string{engine.AllOutputs}, engine.ResultOptions{
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

				outputs := res.Artifacts

				// TODO how to render res natively without exec
				_, err = hbbtexec.Run(m.Exec, send, func(args hbbtexec.RunArgs) (*engine.ExecuteChResult, error) {
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
