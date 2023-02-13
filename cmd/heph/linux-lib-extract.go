//go:build linux

package main

import (
	"github.com/spf13/cobra"
	lwl "heph/linux-lib"
)

var lleExtra []string

func init() {
	rootCmd.AddCommand(lleCmd)

	lleCmd.Flags().StringArrayVar(&lleExtra, "extra", nil, "extra libs to analyze")
}

var lleCmd = &cobra.Command{
	Use:   "lle",
	Short: "Linux Lib Extract",
	Args:  cobra.ExactArgs(2),
	RunE: func(cmd *cobra.Command, args []string) error {
		binPath := args[0]
		toPath := args[1]

		err := lwl.ExtractLibs(binPath, lleExtra, toPath)
		if err != nil {
			return err
		}

		return nil
	},
}
