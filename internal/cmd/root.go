package cmd

import (
	"context"
	"fmt"
	"github.com/hephbuild/hephv2/internal/hcore/hlog"
	"github.com/mattn/go-isatty"
	"github.com/spf13/cobra"
	"log/slog"
	"os"
	"runtime"
	"runtime/pprof"
)

var plain bool
var debug bool
var cpuprofile string
var cpuProfileFile *os.File
var memprofile string

var levelVar slog.LevelVar

func init() {
	levelVar.Set(slog.LevelDebug)
}

var rootCmd = &cobra.Command{
	Use:              "heph",
	TraverseChildren: true,
	SilenceUsage:     true,
	PersistentPreRunE: func(cmd *cobra.Command, args []string) error {
		if !debug {
			levelVar.Set(slog.LevelInfo)
		}

		if cpuprofile != "" {
			var err error
			cpuProfileFile, err = os.Create(cpuprofile)
			if err != nil {
				return fmt.Errorf("could not create CPU profile: %w", err)
			}
			if err := pprof.StartCPUProfile(cpuProfileFile); err != nil {
				return fmt.Errorf("could not start CPU profile: %w", err)
			}
		}

		return nil
	},
	PersistentPostRun: func(cmd *cobra.Command, args []string) {
		ctx := cmd.Context()

		if cpuProfileFile != nil {
			pprof.StopCPUProfile()
			err := cpuProfileFile.Close()
			if err != nil {
				hlog.From(ctx).Error(fmt.Sprintf("could not close cpu profile: %v", err))
				return
			}
		}

		if memprofile != "" {
			f, err := os.Create(memprofile)
			if err != nil {
				hlog.From(ctx).Error(fmt.Sprintf("could not create memory profile: %v", err))
				return
			}
			defer f.Close()
			runtime.GC() // get up-to-date statistics
			if err := pprof.WriteHeapProfile(f); err != nil {
				hlog.From(ctx).Error(fmt.Sprintf("could not write memory profile: %v", err))
			}
		}
	},
}

func init() {
	isTerm := isatty.IsTerminal(os.Stderr.Fd())

	rootCmd.PersistentFlags().BoolVarP(&plain, "plain", "", !isTerm, "disable terminal UI")
	rootCmd.PersistentFlags().BoolVarP(&debug, "debug", "", false, "enable debug log")

	rootCmd.PersistentFlags().StringVar(&cpuprofile, "cpuprofile", "", "CPU Profile file")
	rootCmd.PersistentFlags().StringVar(&memprofile, "memprofile", "", "Memory Profile file")
}

func Execute() {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	logger := hlog.NewTextLogger(os.Stderr, &levelVar)
	ctx = hlog.ContextWithLogger(ctx, logger)

	if err := rootCmd.ExecuteContext(ctx); err != nil {
		logger.Error(err.Error())
		os.Exit(1)
	}
}
