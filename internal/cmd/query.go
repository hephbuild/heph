//nolint:forbidigo
package cmd

import (
	"github.com/spf13/cobra"
)

var queryCmd = func() *cobra.Command {
	return &cobra.Command{
		Use:     "query",
		Aliases: []string{"q"},
		Args:    cobra.NoArgs,
	}
}()

func init() {
	rootCmd.AddCommand(queryCmd)
}
