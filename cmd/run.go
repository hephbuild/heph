package cmd

import (
	"github.com/hephbuild/hephv2/engine"
	"github.com/spf13/cobra"
)

func init() {
	var shell bool

	var runCmd = &cobra.Command{
		Use: "run",
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
		},
	}

	runCmd.Flags().BoolVarP(&shell, "shell", "", false, "shell into target")

	rootCmd.AddCommand(runCmd)
}
