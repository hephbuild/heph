package cmd

import (
	"github.com/spf13/cobra"
)

var inspectCmd *cobra.Command

func init() {
	inspectCmd = &cobra.Command{
		Use:     "inspect",
		Short:   "Inspect the universe of targets",
		Aliases: []string{"i"},
	}

	rootCmd.AddCommand(inspectCmd)
}
