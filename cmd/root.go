package cmd

import (
	"context"
	"github.com/hephbuild/hephv2/hio"
	"github.com/mattn/go-isatty"
	"github.com/spf13/cobra"
	"log/slog"
	"os"
)

var plain bool
var debug bool

var levelVar slog.LevelVar

func init() {
	levelVar.Set(slog.LevelDebug)
}

var rootCmd = &cobra.Command{
	Use:              "heph",
	TraverseChildren: true,
	PersistentPreRunE: func(cmd *cobra.Command, args []string) error {
		if !debug {
			levelVar.Set(slog.LevelInfo)
		}

		return nil
	},
}

func init() {
	isTerm := isatty.IsTerminal(os.Stderr.Fd())

	rootCmd.PersistentFlags().BoolVarP(&plain, "plain", "", !isTerm, "disable terminal UI")
	rootCmd.PersistentFlags().BoolVarP(&debug, "debug", "", false, "enable debug log")
}

func Execute() {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	ctx = hio.ContextWithHandler(ctx, hio.NewDefaultHandler())
	lh := hio.NewTermLogHandler(ctx, os.Stderr, &levelVar)
	defer lh.Close()

	if err := rootCmd.ExecuteContext(ctx); err != nil {
		hio.From(ctx).Err(err.Error())
		os.Exit(1)
	}
}
