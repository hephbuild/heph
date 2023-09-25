//go:build linux

package main

import (
	lwl "github.com/hephbuild/heph/linux-lib"
	"github.com/spf13/cobra"
)

var lleExtra []string

func init() {
	rootCmd.AddCommand(lleCmd)

	lleCmd.Flags().StringArrayVar(&lleExtra, "extra", nil, "extra libs to analyze")
}

var lleCmd = &cobra.Command{
	Use:   "lle",
	Short: "Linux Lib Extract",
	Args:  cobra.ExactArgs(3),
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		binPath := args[0]
		libPath := args[1]
		ldPath := args[2]

		err := lwl.ExtractLibs(ctx, binPath, lleExtra, libPath, ldPath)
		if err != nil {
			return err
		}

		return nil
	},
}
