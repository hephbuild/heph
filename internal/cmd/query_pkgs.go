//nolint:forbidigo
package cmd

import (
	"fmt"
	"github.com/hephbuild/heph/tmatch"
	"os"
	"os/signal"
	"path/filepath"
	"slices"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/spf13/cobra"
)

func init() {
	cmd := &cobra.Command{
		Use:     "packages",
		Aliases: []string{"pkgs"},
		Args:    cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx := cmd.Context()

			ctx, stop := signal.NotifyContext(ctx, os.Interrupt)
			defer stop()

			cwd, err := engine.Cwd()
			if err != nil {
				return err
			}

			root, err := engine.Root()
			if err != nil {
				return err
			}

			e, err := newEngine(ctx, root)
			if err != nil {
				return err
			}

			pkgMatcher, err := tmatch.ParsePackageMatcher(args[0], cwd, root)
			if err != nil {
				return err
			}

			packages := tmatch.Packages(root, pkgMatcher, func(path string) bool {
				if path == e.Home.Path() {
					return false
				}

				if slices.Contains([]string{"node_modules", "dist"}, filepath.Base(path)) {
					return false
				}

				return true
			})

			for pkg, err := range packages {
				if err != nil {
					return err
				}

				if err := ctx.Err(); err != nil {
					return err
				}

				fmt.Println(pkg)
			}

			return nil

		},
	}

	queryCmd.AddCommand(cmd)
}
