package cmd

import (
	"context"
	"errors"
	"fmt"
	"github.com/mattn/go-isatty"
	log "github.com/sirupsen/logrus"
	"github.com/spf13/cobra"
	"go.opentelemetry.io/otel/exporters/jaeger"
	"go.opentelemetry.io/otel/sdk/resource"
	tracesdk "go.opentelemetry.io/otel/sdk/trace"
	semconv "go.opentelemetry.io/otel/semconv/v1.12.0"
	"go.uber.org/multierr"
	"heph/config"
	"heph/engine"
	"heph/targetspec"
	"heph/utils"
	"heph/worker"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"runtime"
	"runtime/pprof"
	"strings"
	"syscall"
	"time"
)

var isTerm bool

var logLevel *string
var profiles *[]string
var plain *bool
var noGen *bool
var porcelain *bool
var workers *int
var cpuprofile *string
var memprofile *string
var shell *bool
var nocache *bool
var params *[]string
var summary *bool
var jaegerEndpoint *string

func init() {
	// Output to stdout instead of the default stderr
	// Can be any io.Writer, see below for File example
	log.SetOutput(os.Stderr)

	if os.Stderr != nil {
		isTerm = isatty.IsTerminal(os.Stderr.Fd())
	}

	log.SetFormatter(&log.TextFormatter{
		DisableTimestamp: isTerm,
		ForceColors:      isTerm,
	})

	log.SetLevel(log.InfoLevel)

	cleanCmd.AddCommand(cleanLockCmd)

	shell = runCmd.Flags().Bool("shell", false, "Opens a shell with the environment setup")

	rootCmd.AddCommand(runCmd)
	rootCmd.AddCommand(watchCmd)
	rootCmd.AddCommand(cleanCmd)
	rootCmd.AddCommand(queryCmd)
	rootCmd.AddCommand(gcCmd)

	cpuprofile = rootCmd.PersistentFlags().String("cpuprofile", "", "CPU Profile file")
	memprofile = rootCmd.PersistentFlags().String("memprofile", "", "Mem Profile file")
	logLevel = rootCmd.PersistentFlags().String("log_level", log.InfoLevel.String(), "log level")
	profiles = rootCmd.PersistentFlags().StringArray("profile", config.ProfilesFromEnv(), "config profiles")
	porcelain = rootCmd.PersistentFlags().Bool("porcelain", false, "Machine readable output, disables all logging")
	nocache = rootCmd.PersistentFlags().Bool("no-cache", false, "Disables cache")
	summary = rootCmd.PersistentFlags().Bool("summary", false, "Prints execution stats")
	jaegerEndpoint = rootCmd.PersistentFlags().String("jaeger", "", "Jaeger endpoint to collect traces")

	plain = rootCmd.PersistentFlags().Bool("plain", false, "Plain output")
	workers = rootCmd.PersistentFlags().Int("workers", runtime.NumCPU(), "Number of workers")
	noGen = rootCmd.PersistentFlags().Bool("no-gen", false, "Disable generated targets")
	params = rootCmd.PersistentFlags().StringArrayP("param", "p", nil, "Set parameter name=value")

	rootCmd.Flags().SetInterspersed(false)
	setupRootUsage()
}

var cpuProfileFile *os.File

var Engine *engine.Engine

var rootCmd = &cobra.Command{
	Use:           "heph",
	Short:         "Efficient build system",
	Version:       utils.Version,
	SilenceUsage:  true,
	SilenceErrors: true,
	Args:          cobra.ArbitraryArgs,
	PersistentPreRunE: func(cmd *cobra.Command, args []string) error {
		lvl, err := log.ParseLevel(*logLevel)
		if err != nil {
			return err
		}

		log.SetLevel(lvl)

		if *porcelain {
			switchToPorcelain()
		}

		if *cpuprofile != "" {
			cpuProfileFile, err = os.Create(*cpuprofile)
			if err != nil {
				return fmt.Errorf("could not create CPU profile: %w", err)
			}
			if err := pprof.StartCPUProfile(cpuProfileFile); err != nil {
				return fmt.Errorf("could not start CPU profile: %w", err)
			}
		}

		return nil
	},
	PersistentPostRunE: func(cmd *cobra.Command, args []string) error {
		defer func() {
			if cpuProfileFile != nil {
				pprof.StopCPUProfile()
				defer cpuProfileFile.Close()
			}

			if *memprofile != "" {
				f, err := os.Create(*memprofile)
				if err != nil {
					log.Errorf("could not create memory profile: %v", err)
					return
				}
				defer f.Close()
				runtime.GC() // get up-to-date statistics
				if err := pprof.WriteHeapProfile(f); err != nil {
					log.Errorf("could not write memory profile: %v", err)
				}
			}
		}()

		if Engine == nil {
			return nil
		}

		if !Engine.Pool.IsDone() {
			log.Tracef("Waiting for all pool items to finish")
			<-Engine.Pool.Done()
			log.Tracef("All pool items finished")
		}

		Engine.Pool.Stop(nil)

		err := Engine.Pool.Err()
		if err != nil {
			log.Error(err)
		}

		Engine.RunExitHandlers()

		if *summary {
			PrintSummary(Engine.TraceRecorder)
		}

		return nil
	},
	ValidArgsFunction: func(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective) {
		_, _, err := preRunAutocomplete(cmd.Context())
		if err != nil {
			return nil, cobra.ShellCompDirectiveError
		}

		aliases := Engine.GetTargetShortcuts()

		names := make([]string, 0)
		for _, target := range aliases {
			names = append(names, target.Name)
		}

		return names, cobra.ShellCompDirectiveNoFileComp
	},
	RunE: func(cmd *cobra.Command, args []string) error {
		if len(args) == 0 {
			_ = cmd.Help()
			return nil
		}

		switchToPorcelain()

		err := preRunWithGen(cmd.Context(), false)
		if err != nil {
			return err
		}

		alias := args[0]

		target := Engine.Targets.Find("//:" + alias)
		if target == nil {
			return fmt.Errorf("alias %v not defined\n", alias)
		}

		err = run(cmd.Context(), Engine, []engine.TargetRunRequest{{Target: target, Args: args[1:]}}, true)
		if err != nil {
			return err
		}

		return nil
	},
}

var runCmd = &cobra.Command{
	Use:               "run",
	Aliases:           []string{"r"},
	Short:             "Run target",
	SilenceUsage:      true,
	SilenceErrors:     true,
	Args:              cobra.MinimumNArgs(1),
	ValidArgsFunction: ValidArgsFunctionTargets,
	RunE: func(cmd *cobra.Command, args []string) error {
		if hasStdin(args) {
			tps, err := parseTargetPathsFromStdin()
			if err != nil {
				return err
			}

			if len(tps) == 0 {
				return nil
			}
		}

		err := preRunWithGen(cmd.Context(), false)
		if err != nil {
			return err
		}

		rrs, err := parseTargetsAndArgs(Engine, args)
		if err != nil {
			return err
		}

		if !hasStdin(args) && len(rrs) == 0 {
			_ = cmd.Help()
			return nil
		}

		fromStdin := hasStdin(args)

		err = run(cmd.Context(), Engine, rrs, !fromStdin)
		if err != nil {
			return err
		}

		return nil
	},
}

func switchToPorcelain() {
	log.Tracef("Switching to porcelain")
	*porcelain = true
	*plain = true
	log.SetLevel(log.ErrorLevel)
}

func preRunAutocomplete(ctx context.Context) ([]string, []string, error) {
	return preRunAutocompleteInteractive(ctx, false, true)
}

func preRunAutocompleteInteractive(ctx context.Context, includePrivate, silent bool) ([]string, []string, error) {
	err := engineInit()
	if err != nil {
		return nil, nil, err
	}

	cache, _ := Engine.LoadAutocompleteCache()

	if cache != nil {
		if includePrivate {
			return cache.AllTargets, cache.Labels, nil
		}

		return cache.PublicTargets, cache.Labels, nil
	}

	err = preRunWithGen(ctx, silent)
	if err != nil {
		return nil, nil, err
	}

	return Engine.Targets.FQNs(), Engine.Labels, nil
}

func findRoot() (string, error) {
	root, err := filepath.Abs(".")
	if err != nil {
		return "", err
	}

	if cwd := os.Getenv("HEPH_CWD"); cwd != "" {
		root = cwd
	}

	parts := strings.Split(root, string(filepath.Separator))
	for len(parts) > 0 {
		p := "/" + filepath.Join(parts...)

		if _, err := os.Stat(filepath.Join(p, ".hephconfig")); err == nil {
			return p, nil
		}

		parts = parts[:len(parts)-1]
	}

	return "", fmt.Errorf("root not found, are you running this command in the repo directory?")
}

func engineFactory() (*engine.Engine, error) {
	root, err := findRoot()
	if err != nil {
		return nil, err
	}

	log.Tracef("Root: %v", root)

	e := engine.New(root)

	return e, nil
}

func engineInit() error {
	if Engine == nil {
		var err error
		Engine, err = engineFactory()
		if err != nil {
			return err
		}
	}

	return engineInitWithEngine(Engine)
}

func engineInitWithEngine(e *engine.Engine) error {
	if e.RanInit {
		return nil
	}
	e.RanInit = true

	if *summary || *jaegerEndpoint != "" {

		opts := []tracesdk.TracerProviderOption{
			tracesdk.WithResource(resource.NewWithAttributes(
				semconv.SchemaURL,
				semconv.ServiceNameKey.String("heph"),
			)),
		}

		if *jaegerEndpoint != "" {
			jexp, err := jaeger.New(jaeger.WithCollectorEndpoint(jaeger.WithEndpoint(*jaegerEndpoint)))
			if err != nil {
				return err
			}

			opts = append(opts, tracesdk.WithBatcher(jexp))
		}

		if *summary {
			opts = append(opts, tracesdk.WithSpanProcessor(e.TraceRecorder))
		}

		pr := tracesdk.NewTracerProvider(opts...)
		e.Tracer = pr.Tracer("heph")

		e.StartRootSpan()
		e.RegisterExitHandler(func() {
			e.RootSpan.End()
			_ = pr.ForceFlush(context.Background())
			_ = pr.Shutdown(context.Background())
		})
	}

	e.Config.Profiles = *profiles

	err := e.Init()
	if err != nil {
		return err
	}

	paramsm := map[string]string{}
	for k, v := range e.Config.Params {
		paramsm[k] = v
	}
	for _, s := range *params {
		parts := strings.SplitN(s, "=", 2)
		if len(parts) != 2 {
			return fmt.Errorf("parameter must be name=value, got `%v`", s)
		}

		paramsm[parts[0]] = parts[1]
	}
	e.Params = paramsm
	e.Pool = worker.NewPool(*workers)
	e.RegisterExitHandler(func() {
		e.Pool.Stop(nil)
	})

	err = e.Parse()
	if err != nil {
		return err
	}

	return nil
}

func preRunWithGen(ctx context.Context, silent bool) error {
	err := engineInit()
	if err != nil {
		return err
	}

	err = preRunWithGenWithEngine(ctx, Engine, silent)
	if err != nil {
		return err
	}

	return nil
}

func preRunWithGenWithEngine(ctx context.Context, e *engine.Engine, silent bool) error {
	err := engineInitWithEngine(e)
	if err != nil {
		return err
	}

	if *noGen {
		log.Info("Generated targets disabled")
		return nil
	}

	deps, err := e.ScheduleGenPass(ctx)
	if err != nil {
		return err
	}

	err = WaitPool("PreRun gen", e.Pool, deps, silent)
	if err != nil {
		return err
	}

	return nil
}

var cleanCmd = &cobra.Command{
	Use:               "clean",
	Short:             "Clean",
	ValidArgsFunction: ValidArgsFunctionTargets,
	RunE: func(cmd *cobra.Command, args []string) error {
		err := preRunWithGen(cmd.Context(), false)
		if err != nil {
			return err
		}

		var targets []*engine.Target
		if hasStdin(args) {
			var err error
			targets, err = parseTargetsFromStdin(Engine)
			if err != nil {
				return err
			}
		} else {
			for _, arg := range args {
				tp, err := targetspec.TargetParse("", arg)
				if err != nil {
					return err
				}

				target := Engine.Targets.Find(tp.Full())
				if target == nil {
					return engine.TargetNotFoundError(tp.Full())
				}

				targets = append(targets, target)
			}
		}

		for _, target := range targets {
			log.Tracef("Cleaning %v...", target.FQN)
			err := Engine.CleanTarget(target, true)
			if err != nil {
				return err
			}
		}

		return nil
	},
}

var gcCmd = &cobra.Command{
	Use:               "gc",
	Short:             "GC",
	ValidArgsFunction: ValidArgsFunctionTargets,
	RunE: func(cmd *cobra.Command, args []string) error {
		err := preRunWithGen(cmd.Context(), false)
		if err != nil {
			return err
		}

		return Engine.GC(log.Infof, false)
	},
}

var cleanLockCmd = &cobra.Command{
	Use:   "lock",
	Short: "Clean locks",
	Args:  cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		err := preRunWithGen(cmd.Context(), false)
		if err != nil {
			return err
		}

		for _, target := range Engine.Targets.Slice() {
			err := Engine.CleanTargetLock(target)
			if err != nil {
				return err
			}
		}

		return nil
	},
}

func execute() error {
	ctx, cancel := context.WithCancel(context.Background())

	sig := make(chan os.Signal, 1)
	signal.Notify(sig, os.Interrupt, syscall.SIGTERM)

	go func() {
		<-sig
		go func() {
			<-time.After(time.Second)
			log.Warnf("Attempting to soft cancel... ctrl+c one more time to force")
		}()
		cancel()

		<-sig
		os.Exit(1)
	}()

	if err := rootCmd.ExecuteContext(ctx); err != nil {
		return err
	}

	return nil
}

func humanPrintError(err error) {
	errs := multierr.Errors(err)

	for i, err := range errs {
		if i != 0 {
			fmt.Fprintln(os.Stderr)
		}

		var terr engine.TargetFailedError
		if errors.As(err, &terr) {
			log.Errorf("%v failed", terr.Target.FQN)

			var lerr engine.ErrorWithLogFile
			if errors.As(err, &lerr) {
				log.Error(lerr.Error())

				logFile := lerr.LogFile
				info, _ := os.Stat(logFile)
				if info.Size() > 0 {
					fmt.Fprintln(os.Stderr)
					c := exec.Command("cat", logFile)
					c.Stdout = os.Stderr
					_ = c.Run()
					fmt.Fprintln(os.Stderr)
					fmt.Fprintf(os.Stderr, "The log file can be found at %v\n", logFile)
				}
			} else {
				log.Error(terr.Error())
			}
		} else {
			log.Error(err)
		}
	}

	if len(errs) > 1 {
		fmt.Fprintln(os.Stderr)
		log.Errorf("%v jobs failed", len(errs))
	}
}

func Execute() {
	utils.Seed()

	if err := execute(); err != nil {
		humanPrintError(err)

		var eerr ErrorWithExitCode
		if errors.As(err, &eerr) {
			os.Exit(eerr.ExitCode)
		}
		os.Exit(1)
	}
}
