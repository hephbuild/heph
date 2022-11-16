package cmd

import (
	"fmt"
	"github.com/mattn/go-isatty"
	log "github.com/sirupsen/logrus"
	"github.com/spf13/cobra"
	"heph/config"
	"heph/engine"
	"heph/utils"
	"os"
	"path/filepath"
	"runtime"
	"runtime/pprof"
	"strings"
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

func postRun() {
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
		return
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
}

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

		cobra.OnFinalize(postRun)

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
		rrs, err := parseTargetsAndArgs(cmd.Context(), args)
		if err != nil {
			return err
		}

		fromStdin := hasStdin(args)

		if len(rrs) == 0 {
			if fromStdin {
				_ = cmd.Help()
			}
			return nil
		}

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

var cleanCmd = &cobra.Command{
	Use:               "clean",
	Short:             "Clean",
	ValidArgsFunction: ValidArgsFunctionTargets,
	RunE: func(cmd *cobra.Command, args []string) error {
		rrs, err := parseTargetsAndArgs(cmd.Context(), args)
		if err != nil {
			return err
		}

		targets := rrs.Targets()

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
