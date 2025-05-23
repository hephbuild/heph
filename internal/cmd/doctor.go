package cmd

import (
	"fmt"
	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/plugin/tref"
	"github.com/spf13/cobra"
)

var doctorCmd = func() *cobra.Command {
	printOrErr := func(key, value string, err error) {
		if err != nil {
			fmt.Println(key, err)
		} else {
			fmt.Println(key, value)
		}
	}

	return &cobra.Command{
		Use:  "doctor",
		Args: cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx := cmd.Context()

			ctx, stop := newSignalNotifyContext(ctx)
			defer stop()

			cwd, err := engine.Cwd()
			printOrErr("Working dir:", cwd, err)

			root, err := engine.Root()
			printOrErr("Root dir:", root, err)

			cwp, err := tref.DirToPackage(cwd, root)
			printOrErr("Current package:", cwp, err)

			return nil
		},
	}
}()

func init() {
	rootCmd.AddCommand(doctorCmd)
}
