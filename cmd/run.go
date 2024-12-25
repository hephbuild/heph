package cmd

import (
	"connectrpc.com/connect"
	"fmt"
	"github.com/hephbuild/hephv2/engine"
	"github.com/hephbuild/hephv2/hfs"
	"github.com/hephbuild/hephv2/plugin/c2"
	"github.com/hephbuild/hephv2/plugin/gen/heph/plugin/v1/pluginv1connect"
	"github.com/hephbuild/hephv2/plugin/pluginbuildfile"
	"github.com/hephbuild/hephv2/plugin/pluginexec"
	"github.com/spf13/cobra"
	"net"
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

			mux := http.NewServeMux()

			for _, p := range []pluginv1connect.ProviderHandler{pluginbuildfile.New(hfs.NewOS(root))} {
				path, h := pluginv1connect.NewProviderHandler(p, connect.WithInterceptors(c2.NewInterceptor()))

				mux.Handle(path, h)
			}

			for _, p := range []*pluginexec.Plugin{ /*pluginexec.New(), pluginexec.NewSh(),*/ pluginexec.NewBash()} {
				path, h := pluginv1connect.NewDriverHandler(p, connect.WithInterceptors(c2.NewInterceptor()))
				mux.Handle(path, h)

				path, h = p.PipesHandler()
				mux.Handle(path, h)

				err = e.RegisterDriver(ctx, p)
				if err != nil {
					return err
				}
			}

			readyCh := make(chan string)

			go func() {
				l, err := net.Listen("tcp", "127.0.0.1:")
				if err != nil {
					panic(err)
				}

				readyCh <- "http://" + l.Addr().String()
				close(readyCh)

				if err := http.Serve(l, mux); err != nil {
					panic(err)
				}
			}()

			addr := <-readyCh
			httpClient := http.DefaultClient

			ctx, err = c2.New(ctx, httpClient, addr)
			if err != nil {
				return err
			}

			err = e.RegisterProvider(ctx, pluginv1connect.NewProviderClient(httpClient, addr, connect.WithInterceptors(c2.NewInterceptor())))
			if err != nil {
				return err
			}

			ch := e.Result(ctx, args[0], args[1], []string{engine.AllOutputs}, engine.ResultOptions{
				ExecOptions: engine.ExecOptions{
					HttpClient: httpClient,
					BaseURL:    addr,
					Stdin:      os.Stdin,
					Stdout:     os.Stdout,
					Stderr:     os.Stderr,
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
