package main

import (
	"fmt"
	"github.com/hephbuild/heph/bootstrap"
	"github.com/hephbuild/heph/buildfiles"
	"github.com/hephbuild/heph/config"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/targetrun"
	"github.com/hephbuild/heph/utils"
	"github.com/hephbuild/heph/utils/finalizers"
	"github.com/hephbuild/heph/utils/xstarlark"
	"github.com/spf13/cobra"
	"go.uber.org/multierr"
	"net/http"
	_ "net/http/pprof"
	"os"
	"runtime"
	"runtime/pprof"
)

var logLevel *string
var profiles *[]string
var plain *bool
var noGen *bool
var porcelain *bool
var workers int
var cpuprofile *string
var memprofile *string
var shell *bool
var noCloudTelemetry *bool
var httpPprof *string
var noInline *bool
var printOutput boolStr
var catOutput boolStr
var nocache *bool
var force *bool
var nopty *bool
var alwaysOut *bool
var params *[]string
var summary *bool
var summaryGen *bool
var jaegerEndpoint *string
var check *bool

func getRunOpts() bootstrap.RunOpts {
	return bootstrap.RunOpts{
		NoInline:    *noInline,
		Plain:       *plain,
		PrintOutput: bootstrap.BoolStr{Bool: printOutput.bool, Str: printOutput.str},
		CatOutput:   bootstrap.BoolStr{Bool: catOutput.bool, Str: catOutput.str},
	}
}

func getRROpts() targetrun.RequestOpts {
	return getRROptsX(false)
}

func getRROptsX(pull bool) targetrun.RequestOpts {
	return targetrun.RequestOpts{
		NoCache:       *nocache,
		Force:         *force || *nocache,
		Shell:         *shell,
		PreserveCache: printOutput.bool || catOutput.bool,
		NoPTY:         *nopty,
		PullCache:     pull || *alwaysOut || printOutput.bool || catOutput.bool,
	}
}

func init() {
	log.Setup()

	log.SetLevel(log.InfoLevel)

	shell = runCmd.Flags().Bool("shell", false, "Opens a shell with the environment setup")
	noInline = runCmd.Flags().Bool("no-inline", false, "Force running in workers")
	runCmd.Flags().AddFlag(NewBoolStrFlag(&printOutput, "print-out", "o", "Prints target output paths, --print-out=<name> to filter output"))
	runCmd.Flags().AddFlag(NewBoolStrFlag(&catOutput, "cat-out", "", "Print target output content, --cat-out=<name> to filter output"))
	alwaysOut = runCmd.Flags().Bool("always-out", false, "Ensure output will be present in cache")
	runCmd.Flags().BoolVarP(&all, "all", "a", false, "Force run all")
	runCmd.Flags().MarkHidden("all")

	check = fmtCmd.Flags().Bool("check", false, "Only check formatting")

	rootCmd.AddCommand(runCmd)
	rootCmd.AddCommand(cleanCmd)
	rootCmd.AddCommand(queryCmd)
	rootCmd.AddCommand(cloudCmd)
	rootCmd.AddCommand(gcCmd)
	rootCmd.AddCommand(validateCmd)
	rootCmd.AddCommand(setupCmd)
	rootCmd.AddCommand(searchCmd)
	rootCmd.AddCommand(fmtCmd)

	cpuprofile = rootCmd.PersistentFlags().String("cpuprofile", "", "CPU Profile file")
	memprofile = rootCmd.PersistentFlags().String("memprofile", "", "Mem Profile file")
	logLevel = rootCmd.PersistentFlags().String("log_level", log.InfoLevel.String(), "log level")
	profiles = rootCmd.PersistentFlags().StringArray("profile", config.ProfilesFromEnv(), "config profiles")
	porcelain = rootCmd.PersistentFlags().Bool("porcelain", false, "Machine readable output, disables all logging")
	nocache = rootCmd.PersistentFlags().Bool("no-cache", false, "Disables cache, cache restore and force run")
	force = rootCmd.PersistentFlags().Bool("force", false, "Forcefully run a target")
	nopty = rootCmd.PersistentFlags().Bool("no-pty", false, "Disables PTY")
	summary = rootCmd.PersistentFlags().Bool("summary", false, "Prints execution stats")
	summaryGen = rootCmd.PersistentFlags().Bool("summary-gen", false, "Prints execution stats, including during gen")
	jaegerEndpoint = rootCmd.PersistentFlags().String("jaeger", "", "Jaeger endpoint to collect traces")
	noCloudTelemetry = rootCmd.PersistentFlags().Bool("no-cloud-telemetry", false, "Disable cloud reporting")
	httpPprof = rootCmd.PersistentFlags().String("http-pprof", "", "Http pprof address")

	plain = rootCmd.PersistentFlags().Bool("plain", false, "Plain output")
	rootCmd.PersistentFlags().Var(newWorkersValue(&workers), "workers", "Workers to spawn as a number or percentage")
	noGen = rootCmd.PersistentFlags().Bool("no-gen", false, "Disable generated targets")
	params = rootCmd.PersistentFlags().StringArrayP("param", "p", nil, "Set parameter name=value")

	rootCmd.Flags().SetInterspersed(false)

	rootCmd.PersistentFlags().MarkHidden("cpuprofile")
	rootCmd.PersistentFlags().MarkHidden("memprofile")
	rootCmd.PersistentFlags().MarkHidden("jaeger")
}

var cpuProfileFile *os.File

var Finalizers finalizers.Finalizers

func postRun(err error) {
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

	Finalizers.Run(err)

	log.Cleanup()
}

var rootCmd = &cobra.Command{
	Use:           "heph",
	Short:         "Efficient build system",
	Version:       utils.Version,
	SilenceUsage:  true,
	SilenceErrors: true,
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

		if addr := *httpPprof; addr != "" {
			go func() {
				log.Error(http.ListenAndServe(addr, nil))
			}()
		}

		return nil
	},
	RunE: func(cmd *cobra.Command, args []string) error {
		err := cmd.Help()
		if err != nil {
			return err
		}

		return bootstrap.ErrorWithExitCode{
			ExitCode: 1,
		}
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
		ctx := cmd.Context()

		bs, rrs, err := parseTargetsAndArgs(ctx, args, func(m specs.Matcher) error {
			if !all && m == specs.AllMatcher {
				return fmt.Errorf("you probably don't want to run all your targets, pass -a to force")
			}

			return nil
		})
		if err != nil {
			return err
		}

		fromStdin := bootstrap.HasStdin(args)

		if len(rrs) == 0 {
			log.Error("no target match")
			return bootstrap.ErrorWithExitCode{ExitCode: 1}
		}

		err = bootstrap.Run(ctx, bs.Scheduler, rrs, getRunOpts(), !fromStdin)
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

var cleanCmd = &cobra.Command{
	Use:               "clean",
	Short:             "Clean",
	ValidArgsFunction: ValidArgsFunctionTargets,
	RunE: func(cmd *cobra.Command, args []string) error {
		bs, rrs, err := parseTargetsAndArgs(cmd.Context(), args, nil)
		if err != nil {
			return err
		}

		targets := rrs.Targets()

		for _, target := range targets.Slice() {
			log.Tracef("Cleaning %v...", target.Addr)
			err := bs.Scheduler.CleanTarget(target, true)
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
		bs, err := schedulerWithGenInit(cmd.Context())
		if err != nil {
			return err
		}

		err = bs.Scheduler.LocalCache.GC(cmd.Context(), log.Infof, false)
		if err != nil {
			return err
		}

		return nil
	},
}

var validateCmd = &cobra.Command{
	Use:   "validate",
	Short: "Validate the complete graph",
	Args:  cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		bs, err := schedulerInit(ctx, nil)
		if err != nil {
			return err
		}

		err = linkAll(ctx, bs.Scheduler)
		if err != nil {
			return err
		}

		log.Info("Build graph is valid")

		return nil
	},
}

var setupCmd = &cobra.Command{
	Use:   "setup",
	Short: "Installs tools in path",
	Args:  cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		// bootstrap.BootScheduler installs the tools
		_, err := schedulerInit(ctx, nil)
		if err != nil {
			return err
		}

		return nil
	},
}

var fmtCmd = &cobra.Command{
	Use:   "fmt",
	Short: "Format build files",
	Args:  cobra.ArbitraryArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		bs, err := bootstrapBase(ctx)
		if err != nil {
			return err
		}

		buildfilesState := buildfiles.NewState(buildfiles.State{
			Ignore:   bs.Config.BuildFiles.Ignore,
			Patterns: bs.Config.BuildFiles.Patterns,
		})

		var files []string
		if len(args) == 0 {
			files, err = buildfilesState.CollectFiles(ctx, bs.Root.Root.Abs())
			if err != nil {
				return err
			}
		} else {
			files = args
		}

		cfg := xstarlark.FmtConfig{
			IndentSize: bs.Config.Fmt.IndentSize,
		}

		if *check {
			var errs error
			for _, file := range files {
				err := xstarlark.FmtCheck(file, cfg)
				if err != nil {
					errs = multierr.Append(errs, fmt.Errorf("%v: %w", file, err))
				}
			}

			return errs
		}

		var errs error
		for _, file := range files {
			if file == "-" {
				if len(files) > 1 {
					return fmt.Errorf("stdin only compatible with one input")
				}

				res, err := xstarlark.Fmt("<stdin>", os.Stdin, cfg)
				if err != nil {
					return err
				}

				fmt.Print(res)

				continue
			}

			err := xstarlark.FmtFix(file, cfg)
			if err != nil {
				errs = multierr.Append(errs, fmt.Errorf("%v: %w", file, err))
			}
		}

		return errs
	},
}
