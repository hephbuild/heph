package cmd

import (
	"context"
	"fmt"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/hephbuild/hephv2/internal/engine"
	"github.com/hephbuild/hephv2/internal/hbbt/hbbtexec"
	"github.com/hephbuild/hephv2/internal/hfs"
	"github.com/hephbuild/hephv2/internal/termui"
	"github.com/hephbuild/hephv2/plugin/pluginbuildfile"
	"github.com/hephbuild/hephv2/plugin/pluginexec"
	"github.com/spf13/cobra"
	"io"
	"net/http"
	"os"
	"time"
)

func init() {
	var shell bool

	var runCmd = &cobra.Command{
		Use:  "run",
		Args: cobra.ExactArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx := cmd.Context()

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

				for _, p := range []*pluginexec.Plugin{pluginexec.New(), pluginexec.NewSh(), pluginexec.NewBash()} {
					_, err := e.RegisterDriver(ctx, p, func(mux *http.ServeMux) {
						path, h := p.PipesHandler()
						mux.Handle(path, h)
					})
					if err != nil {
						return err
					}
				}

				ch := e.Result(ctx, args[0], args[1], []string{engine.AllOutputs}, engine.ResultOptions{
					ExecInteractiveCallback: true,
					Shell:                   shell,
				})

				res := <-ch

				if res.Err != nil {
					return res.Err
				}

				outputs := res.Outputs

				if res.ExecInteractive != nil {
					outputs, err = hbbtexec.Run(m.Exec, send, func(stdin io.Reader, stdout, stderr io.Writer) ([]engine.ExecuteResultOutput, error) {
						res := <-res.ExecInteractive(engine.ExecOptions{
							Stdin:  os.Stdin,
							Stdout: os.Stdout,
							Stderr: os.Stderr,
						})

						if res.Err != nil {
							return nil, res.Err
						}

						return res.Outputs, nil
					})
					if err != nil {
						return err
					}
				}

				send(termui.ResetSteps())

				// TODO how to render res natively without exec
				_, err = hbbtexec.Run(m.Exec, send, func(stdin io.Reader, stdout, stderr io.Writer) (*engine.ExecuteResult, error) {
					for _, output := range outputs {
						fmt.Fprintln(stdout, output.Name)
						fmt.Fprintln(stdout, "  group:    ", output.Group)
						fmt.Fprintln(stdout, "  uri:      ", output.Uri)
						fmt.Fprintln(stdout, "  type:     ", output.Type.String())
						fmt.Fprintln(stdout, "  encoding: ", output.Encoding.String())
					}

					return res, nil
				})
				if err != nil {
					return err
				}

				time.Sleep(time.Millisecond)

				return nil
			})
			if err != nil {
				return err
			}

			return nil
		},
	}

	runCmd.Flags().BoolVarP(&shell, "shell", "", false, "shell into target")

	rootCmd.AddCommand(runCmd)
}
