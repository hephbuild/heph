package cmd

import (
	"context"
	"fmt"
	"github.com/hephbuild/hephv2/internal/engine"
	"github.com/hephbuild/hephv2/internal/hcore/hlog"
	"github.com/hephbuild/hephv2/internal/hcore/hstep"
	"github.com/hephbuild/hephv2/internal/hfs"
	"github.com/hephbuild/hephv2/plugin/gen/heph/core/v1"
	"github.com/hephbuild/hephv2/plugin/pluginbuildfile"
	"github.com/hephbuild/hephv2/plugin/pluginexec"
	"github.com/spf13/cobra"
	"net/http"
	"os"
)

func init() {
	var shell bool

	var runCmd = &cobra.Command{
		Use:  "run",
		Args: cobra.ExactArgs(2),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx := cmd.Context()

			root, err := engine.Root()
			if err != nil {
				return err
			}

			ctx = hstep.ContextWithHandler(ctx, func(ctx context.Context, pbstep *corev1.Step) *corev1.Step {
				hlog.From(ctx).Info(fmt.Sprintf("%v %v", pbstep.Text, pbstep.Status.String()))

				return pbstep
			})

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
				ExecOptions: engine.ExecOptions{
					Stdin:  os.Stdin,
					Stdout: os.Stdout,
					Stderr: os.Stderr,
				},
			})

			res := <-ch

			if res.Err != nil {
				return res.Err
			}

			for _, output := range res.Outputs {
				fmt.Println(output.Name)
				fmt.Println("  group:    ", output.Group)
				fmt.Println("  uri:      ", output.Uri)
				fmt.Println("  type:     ", output.Type.String())
				fmt.Println("  encoding: ", output.Encoding.String())
			}

			return nil
		},
	}

	runCmd.Flags().BoolVarP(&shell, "shell", "", false, "shell into target")

	rootCmd.AddCommand(runCmd)
}
