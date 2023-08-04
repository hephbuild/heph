package main

import (
	"github.com/hephbuild/heph/bootstrap"
	"github.com/hephbuild/heph/bootstrapwatch"
	"github.com/spf13/cobra"
)

var watchIgnore *[]string

func init() {
	watchIgnore = watchCmd.Flags().StringArray("ignore", nil, "Ignore files, supports glob")
	rootCmd.AddCommand(watchCmd)
}

var watchCmd = &cobra.Command{
	Use:               "watch",
	Aliases:           []string{"w"},
	Short:             "Watch targets",
	SilenceUsage:      true,
	SilenceErrors:     true,
	Args:              cobra.ArbitraryArgs,
	ValidArgsFunction: ValidArgsFunctionTargets,
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		tps, targs, err := bootstrap.ParseTargetAddrsAndArgs(args, true)
		if err != nil {
			return err
		}

		opts, err := bootstrapOptions()
		if err != nil {
			return err
		}

		bbs, err := bootstrap.BootBase(ctx, opts)
		if err != nil {
			return err
		}

		ws, err := bootstrapwatch.Boot(ctx, bbs.Root, opts, getRunOpts(), getRROpts(), tps, targs, *watchIgnore)
		if err != nil {
			return err
		}
		defer ws.Close()

		return ws.Start()
	},
}
