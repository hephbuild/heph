package cmd

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"

	"github.com/hephbuild/heph/internal/hmaps"
	"github.com/hephbuild/heph/lib/tref"
	"github.com/hephbuild/heph/plugin/pluginfs"

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
		Use:   "doctor",
		Short: "Run a series of checks to diagnose issues related to the environment",
		Args:  cobra.NoArgs,
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

			err = doctorXattr(ctx, root)
			if err != nil {
				fmt.Println("Xattr failed:", err)
			} else {
				fmt.Println("Xattr ok")
			}

			return nil
		},
	}
}()

func doctorXattr(ctx context.Context, root string) error {
	path := filepath.Join(root, "heph_doctor_xattr_test")

	err := os.RemoveAll(path)
	if err != nil {
		return err
	}

	err = os.WriteFile(path, []byte("test"), 0600)
	if err != nil {
		return err
	}
	defer os.Remove(path)

	err = pluginfs.MarkCodegen(ctx, tref.New("", "heph_doctor_test", nil), path)
	if err != nil {
		return err
	}

	if !pluginfs.IsCodegen(ctx, path) {
		return errors.New("xattr not set, something went wrong")
	}

	return err
}

func init() {
	rootCmd.AddCommand(doctorCmd)
}
