package cmd

import (
	"fmt"

	"github.com/hephbuild/heph/internal/hmaps"
	"github.com/hephbuild/heph/lib/tref"

	"github.com/hephbuild/heph/internal/engine"
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

			cwd, err := engine.Cwd()
			printOrErr("Working dir:", cwd, err)

			root, err := engine.Root()
			printOrErr("Root dir:", root, err)

			cwp, err := tref.DirToPackage(cwd, root)
			printOrErr("Current package:", cwp, err)

			e, err := newEngine(ctx, root)
			if err != nil {
				fmt.Println("engine", err)
			} else {
				fmt.Println("Drivers:")
				for name := range hmaps.Sorted(e.DriversByName) {
					fmt.Println(" -", name)
				}
				fmt.Println("Providers:")
				for _, p := range e.Providers {
					fmt.Println(" -", p.Name)
				}
			}

			return nil
		},
	}
}()

func init() {
	rootCmd.AddCommand(doctorCmd)
}
