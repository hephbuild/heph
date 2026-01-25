package cmd

import (
	"context"
	"fmt"
	"log/slog"
	"net/http"
	pprofhttp "net/http/pprof"
	"os"
	"runtime"
	"runtime/pprof"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/hephbuild/heph/internal/hdebug"
	"github.com/hephbuild/heph/internal/hlocks"
	"github.com/hephbuild/heph/internal/hversion"
	"github.com/hephbuild/heph/lib/hcobra"
	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/attribute"
	oteltrace "go.opentelemetry.io/otel/trace"

	"runtime/trace"

	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/mattn/go-isatty"
	"github.com/spf13/cobra"
)

var plain bool
var debug bool
var quiet bool
var pprofCpuPath string
var pprofMemPath string
var pprofGoroutinePath string
var pprofGoroutineLast bool
var pprofServer hcobra.BoolStr
var tracePath string

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

		hlocks.StartGlobalMutexGC()
		registerFinalize(func() {
			hlocks.StopGlobalMutexGC()
		})

		if quiet {
			levelVar.Set(slog.LevelError)
		} else if debug {
			levelVar.Set(slog.LevelDebug)
		} else {
			levelVar.Set(slog.LevelInfo)
		}

		ctx, clean := hdebug.SetLabels(ctx, func() []string {
			return []string{"where", "cmd " + strings.Join(args, " ")}
		})
		defer clean()

		if true {
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

		ctx, rootSpan := tracer.Start(ctx, strings.Join(spanArgs, " "), oteltrace.WithAttributes(attribute.StringSlice("heph.args", spanArgs)))
		registerFinalize(func() {
			rootSpan.End()
		})

		{
			spanCtx := rootSpan.SpanContext()
			if spanCtx.HasTraceID() {
				traceID := spanCtx.TraceID().String()

				hlog.From(ctx).Info("Trace ID: " + traceID)
			} else {
				// add a span context that is not nil so that the noop tracer doesnt allocate new spans going forward
				// see go/pkg/mod/go.opentelemetry.io/otel/trace@v1.35.0/noop/noop.go Tracer.Start
				ctx = oteltrace.ContextWithSpanContext(ctx, rootSpan.SpanContext().WithTraceID(oteltrace.TraceID{0x1}))
			}
		}

		if pprofServer.Bool {
			addr := pprofServer.Str
			if addr == "" {
				addr = ":6060"
			}

			mux := http.NewServeMux()
			mux.HandleFunc("/debug/pprof/", pprofhttp.Index)
			mux.HandleFunc("/debug/pprof/cmdline", pprofhttp.Cmdline)
			mux.HandleFunc("/debug/pprof/profile", pprofhttp.Profile)
			mux.HandleFunc("/debug/pprof/symbol", pprofhttp.Symbol)
			mux.HandleFunc("/debug/pprof/trace", pprofhttp.Trace)

			go func() {
				ctx, clean := hdebug.SetLabels(ctx, func() []string {
					return []string{"where", "pprof server"}
				})
				defer clean()

				h := hdebug.Middleware(mux, func() []string {
					return []string{"where", "pprof server"}
				})

				err := http.ListenAndServe(addr, h) //nolint:gosec
				if err != nil {
					hlog.From(ctx).Error(fmt.Sprintf("pprof server: %v", err))
				}
			}()
		}

		if pprofCpuPath != "" {
			cpuProfileFile, err := os.Create(pprofCpuPath)
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

		if pprofMemPath != "" {
			registerFinalize(func() {
				f, err := os.Create(pprofMemPath)
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

		if pprofGoroutinePath != "" {
			goroutineFile, err := os.Create(pprofGoroutinePath)
			if err != nil {
				return fmt.Errorf("could not create goroutine profile: %w", err)
			}

			var wg sync.WaitGroup
			wg.Go(func() {
				for {
					select {
					case <-ctx.Done():
						return
					case <-time.After(time.Second):
						if pprofGoroutineLast {
							_ = goroutineFile.Truncate(0)
							_, _ = goroutineFile.Seek(0, 0)
						} else {
							_, _ = goroutineFile.WriteString("\n####################\n")
						}

						err := pprof.Lookup("goroutine").WriteTo(goroutineFile, 2)
						if err != nil {
							hlog.From(ctx).Error(fmt.Sprintf("could not write goroutine profile: %v", err))
						}
						continue
					}
				}
			})

			registerFinalize(func() {
				wg.Wait()

				err := goroutineFile.Close()
				if err != nil {
					hlog.From(ctx).Error(fmt.Sprintf("could not close goroutine profile: %v", err))
				}
			})
		}

		if tracePath != "" {
			traceFile, err := os.Create(tracePath)
			if err != nil {
				return fmt.Errorf("could not create trace: %w", err)
			}

			err = trace.Start(traceFile)
			if err != nil {
				return fmt.Errorf("could not start trace: %w", err)
			} else {
				registerFinalize(func() {
					trace.Stop()

					err := traceFile.Close()
					if err != nil {
						hlog.From(ctx).Error(fmt.Sprintf("could not close trace: %v", err))
					}
				})
			}
		}

		return nil
	},
}

func init() {
	hcobra.Setup(rootCmd)

	rootCmd.PersistentFlags().BoolVarP(&plain, "plain", "", false, "disable terminal UI")
	rootCmd.PersistentFlags().BoolVarP(&debug, "debug", "", false, "enable debug log")
	rootCmd.PersistentFlags().BoolVarP(&quiet, "quiet", "q", false, "set log level to error")

	debugFlagSet := hcobra.NewFlagSet("Global Debug Flags")
	debugFlagSet.BoolStrVar(&pprofServer, "pprof-http", "", "Start pprof server")
	debugFlagSet.StringVar(&pprofCpuPath, "pprof-cpu", "", "CPU Profile output file")
	debugFlagSet.StringVar(&pprofMemPath, "pprof-mem", "", "Memory Profile output file")
	debugFlagSet.StringVar(&pprofGoroutinePath, "pprof-goroutine", "", "Goroutine Profile output file")
	debugFlagSet.BoolVar(&pprofGoroutineLast, "pprof-goroutine-last", false, "Goroutine Profile, keep only last")
	debugFlagSet.StringVar(&tracePath, "trace", "", "Trace output file")

	hcobra.AddLocalFlagSet(rootCmd, debugFlagSet)
	hcobra.AddPersistentFlagSet(rootCmd, debugFlagSet)
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
