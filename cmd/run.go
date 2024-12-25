package cmd

import (
	"fmt"
	"github.com/hephbuild/hephv2/engine"
	"github.com/hephbuild/hephv2/hfs"
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

			e, err := engine.New(root, engine.Config{})
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

			//ctx, err = c2.New(ctx, httpClient, addr)
			//if err != nil {
			//	return err
			//}

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
				fmt.Println(output.Group)
				fmt.Println("  ", output.Name)
				fmt.Println("  ", output.Uri)
				fmt.Println("  ", output.Type.String())
				fmt.Println("  ", output.Encoding.String())
			}

			return nil
		},
	}

	runCmd.Flags().BoolVarP(&shell, "shell", "", false, "shell into target")

	rootCmd.AddCommand(runCmd)
}
