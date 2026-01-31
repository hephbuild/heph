package main

import (
	"fmt"

	"github.com/hephbuild/heph/bootstrap"
	"github.com/hephbuild/heph/lsp"
	"github.com/spf13/cobra"
)

func init() {
	lspCommand.AddCommand(servelspCmd)
}

var lspCommand = &cobra.Command{
	Use:     "lsp",
	Aliases: []string{"lsp"},
	Short:   "Heph Language Server",
	Args:    cobra.RangeArgs(0, 1),
	RunE: func(cmd *cobra.Command, args []string) error {
		return fmt.Errorf("run heph Language Server with `serve`")
	},
}

var servelspCmd = &cobra.Command{
	Use:               "serve",
	Short:             "Serve LSP",
	Aliases:           []string{"s"},
	ValidArgsFunction: ValidArgsFunctionTargets,
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		localOpt := bootstrap.BootOpts{}
		bs, err := bootstrap.BootBase(ctx, localOpt)
		if err != nil {
			return err
		}

		server, err := lsp.NewHephServer(bs.Root)
		if err != nil {
			return err
		}

		return server.Serve()
	},
}
