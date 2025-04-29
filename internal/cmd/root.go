package cmd

import (
	"context"
	"fmt"
	"log/slog"
	"os"
	"runtime"
	"runtime/pprof"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/hephbuild/heph/internal/hversion"
	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/trace"

	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/mattn/go-isatty"
	"github.com/spf13/cobra"
)

var plain bool
var debug bool
var cpuprofile string
var memprofile string
var goroutineprofile string

var levelVar slog.LevelVar

var isTerm = sync.OnceValue(func() bool {
	return isatty.IsTerminal(os.Stderr.Fd())
})

func init() {
	levelVar.Set(slog.LevelDebug)
}

var tracer = otel.Tracer("heph")

var onFinalize []func()

func registerFinalize(f func()) {
	onFinalize = append(onFinalize, f)
}

func runFinalize() {
	for _, f := range slices.Backward(onFinalize) {
		f()
	}
	onFinalize = nil
}

var rootCmd = &cobra.Command{
	Use:              "heph",
	TraverseChildren: true,
	SilenceUsage:     true,
	SilenceErrors:    true,
	Version:          hversion.Version,
	PersistentPreRunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()
		defer func() {
			cmd.SetContext(ctx)
		}()

		if !debug {
			levelVar.Set(slog.LevelInfo)
		}

		if true {
			var err error
			otelShutdown, err := setupOTelSDK(ctx)
			if err != nil {
				return err
			}
			registerFinalize(func() {
				err := otelShutdown(context.WithoutCancel(ctx))
				if err != nil {
					hlog.From(ctx).Error(fmt.Sprintf("otel: %v", err))
				}
			})
		}

		spanArgs := []string{"heph"}
		spanArgs = append(spanArgs, os.Args[1:]...)

		ctx, rootSpan := tracer.Start(ctx, strings.Join(spanArgs, " "), trace.WithAttributes(attribute.StringSlice("heph.args", spanArgs)))
		registerFinalize(func() {
			rootSpan.End()
		})

		{
			spanCtx := rootSpan.SpanContext()
			if spanCtx.HasTraceID() {
				traceID := spanCtx.TraceID().String()

				hlog.From(ctx).Info("Trace ID: " + traceID)
			}
		}

		if cpuprofile != "" {
			cpuProfileFile, err := os.Create(cpuprofile)
			if err != nil {
				return fmt.Errorf("could not create CPU profile: %w", err)
			}
			if err := pprof.StartCPUProfile(cpuProfileFile); err != nil {
				return fmt.Errorf("could not start CPU profile: %w", err)
			}

			registerFinalize(func() {
				pprof.StopCPUProfile()
				err := cpuProfileFile.Close()
				if err != nil {
					hlog.From(ctx).Error(fmt.Sprintf("could not close cpu profile: %v", err))
				}
			})
		}

		if memprofile != "" {
			registerFinalize(func() {
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
			})
		}

		if goroutineprofile != "" {
			goroutineFile, err := os.Create(goroutineprofile)
			if err != nil {
				return fmt.Errorf("could not create goroutine profile: %w", err)
			}

			var wg sync.WaitGroup
			wg.Add(1)
			go func() {
				defer wg.Done()

				for {
					select {
					case <-ctx.Done():
						return
					case <-time.After(time.Second):
						_, _ = goroutineFile.WriteString("\n####################\n")

						err := pprof.Lookup("goroutine").WriteTo(goroutineFile, 2)
						if err != nil {
							hlog.From(ctx).Error(fmt.Sprintf("could not write goroutine profile: %v", err))
						}
						continue
					}
				}
			}()

			registerFinalize(func() {
				wg.Wait()

				err := goroutineFile.Close()
				if err != nil {
					hlog.From(ctx).Error(fmt.Sprintf("could not close goroutine profile: %v", err))
				}
			})
		}

		return nil
	},
}

func init() {
	rootCmd.PersistentFlags().BoolVarP(&plain, "plain", "", false, "disable terminal UI")
	rootCmd.PersistentFlags().BoolVarP(&debug, "debug", "", false, "enable debug log")

	rootCmd.PersistentFlags().StringVar(&cpuprofile, "cpuprofile", "", "CPU Profile file")
	rootCmd.PersistentFlags().StringVar(&memprofile, "memprofile", "", "Memory Profile file")
	rootCmd.PersistentFlags().StringVar(&goroutineprofile, "goroutineprofile", "", "Goroutine Profile file")
}

func Execute() int {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	defer runFinalize()

	logger := hlog.NewTextLogger(os.Stderr, &levelVar)
	ctx = hlog.ContextWithLogger(ctx, logger)

	if err := rootCmd.ExecuteContext(ctx); err != nil {
		logger.Error(err.Error())
		return 1
	}

	return 0
}
