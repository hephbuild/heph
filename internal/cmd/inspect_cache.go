package cmd

import (
	"errors"
	"fmt"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/spf13/cobra"
)

var cacheCmd *cobra.Command

func init() {
	cacheCmd = &cobra.Command{
		Use:   "cache",
		Short: "Cache utilities",
	}

	inspectCmd.AddCommand(cacheCmd)
}

func init() {
	cmdArgs := parseRefArgs{cmdName: "list-versions"}

	cmd := &cobra.Command{
		Use:               cmdArgs.Use(),
		Short:             "List versions for hashin",
		Args:              cmdArgs.Args(),
		ValidArgsFunction: cmdArgs.ValidArgsFunction(),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx := cmd.Context()

			ctx, stop := newSignalNotifyContext(ctx)
			defer stop()

			cwd, err := engine.Cwd()
			if err != nil {
				return err
			}

			root, err := engine.Root()
			if err != nil {
				return err
			}

			ref, err := cmdArgs.Parse(args[0], cwd, root)
			if err != nil {
				return err
			}

			e, err := newEngine(ctx, root)
			if err != nil {
				return err
			}

			for hashin, err := range e.CacheSmall.ListVersions(ctx, ref) {
				if err != nil {
					return err
				}

				fmt.Println(hashin)
			}

			return nil
		},
	}

	cacheCmd.AddCommand(cmd)
}

func init() {
	cmdArgs := parseRefArgs{cmdName: "list-artifacts"}

	cmd := &cobra.Command{
		Use:   cmdArgs.Use(),
		Short: "List artifacts at revision",
		Args: func(cmd *cobra.Command, args []string) error {
			switch len(args) {
			case 2:
				return nil
			default:
				return errors.New("must be `list-artifacts <target-addr> <revision>`")
			}
		},
		ValidArgsFunction: cmdArgs.ValidArgsFunction(),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx := cmd.Context()

			ctx, stop := newSignalNotifyContext(ctx)
			defer stop()

			cwd, err := engine.Cwd()
			if err != nil {
				return err
			}

			root, err := engine.Root()
			if err != nil {
				return err
			}

			ref, err := cmdArgs.Parse(args[0], cwd, root)
			if err != nil {
				return err
			}

			hashin := args[1]

			e, err := newEngine(ctx, root)
			if err != nil {
				return err
			}

			for i, cache := range [...]engine.LocalCache{e.CacheSmall, e.CacheLarge} {
				fmt.Println("======", i)

				for name, err := range cache.ListArtifacts(ctx, ref, hashin) {
					if err != nil {
						return err
					}

					fmt.Println(name)
				}
			}

			return nil
		},
	}

	cacheCmd.AddCommand(cmd)
}
