package cmd

import (
	"fmt"

	"github.com/hephbuild/heph/internal/hversion"
	"github.com/spf13/cobra"
)

func init() {
	cmd := &cobra.Command{
		Use:   "version",
		Short: "Print version",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			fmt.Println(hversion.Version())

			return nil
		},
	}

	rootCmd.AddCommand(cmd)
}
