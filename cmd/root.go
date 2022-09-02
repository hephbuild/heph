package cmd

import (
	"context"
	"errors"
	"fmt"
	"github.com/mattn/go-isatty"
	log "github.com/sirupsen/logrus"
	"github.com/spf13/cobra"
	"heph/config"
	"heph/engine"
	"heph/utils"
	"heph/worker"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"runtime"
	"strings"
	"syscall"
)

var isTerm bool

var logLevel *string
var profiles *[]string
var plain *bool
var noGen *bool
var porcelain *bool
var workers *int

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

	rootCmd.AddCommand(runCmd)
	rootCmd.AddCommand(cleanCmd)
	rootCmd.AddCommand(queryCmd)

	logLevel = rootCmd.PersistentFlags().String("log_level", log.InfoLevel.String(), "log level")
	profiles = rootCmd.PersistentFlags().StringArray("profile", config.ProfilesFromEnv(), "config profiles")
	porcelain = rootCmd.PersistentFlags().Bool("porcelain", false, "Machine readable output, disables all logging")

	plain = rootCmd.PersistentFlags().Bool("plain", false, "Plain output")
	workers = rootCmd.PersistentFlags().Int("workers", runtime.NumCPU(), "Number of workers")
	noGen = rootCmd.PersistentFlags().Bool("no-gen", false, "Disable generated targets")

	rootCmd.Flags().SetInterspersed(false)
	setupRootUsage()
}

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

		return nil
	},
	PersistentPostRunE: func(cmd *cobra.Command, args []string) error {
		if Engine == nil {
			return nil
		}

		log.Tracef("Waiting for all pool items to finish")
		<-Engine.Pool.Done()

		return Engine.Pool.Err()
	},
	ValidArgsFunction: func(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective) {
		err := preRunAutocomplete()
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
	Run: func(cmd *cobra.Command, args []string) {
		if len(args) == 0 {
			_ = cmd.Help()
			return
		}

		switchToPorcelain()

		err := preRunWithGen(false)
		if err != nil {
			log.Fatal(err)
		}

		alias := args[0]

		target := Engine.Targets.Find("//:" + alias)
		if target == nil {
			log.Fatalf("alias %v not defined\n", alias)
		}

		err = run(cmd.Context(), []TargetInvocation{{Target: target, Args: args[1:]}}, false)
		if err != nil {
			log.Fatal(err)
		}
	},
}

var runCmd = &cobra.Command{
	Use:           "run",
	Short:         "Run target",
	SilenceUsage:  true,
	SilenceErrors: true,
	Args:          cobra.ArbitraryArgs,
	ValidArgsFunction: func(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective) {
		err := preRunAutocomplete()
		if err != nil {
			return nil, cobra.ShellCompDirectiveError
		}

		return autocompleteTargetName(Engine.Targets, toComplete), cobra.ShellCompDirectiveNoFileComp
	},
	Run: func(cmd *cobra.Command, args []string) {
		if hasStdin(args) {
			tps, err := parseTargetPathsFromStdin()
			if err != nil {
				log.Fatal(err)
			}

			if len(tps) == 0 {
				return
			}
		}

		err := preRunWithGen(false)
		if err != nil {
			log.Fatal(err)
		}

		targets, err := parseTargetsAndArgs(args)
		if err != nil {
			log.Fatal(err)
		}

		if !hasStdin(args) && len(targets) == 0 {
			_ = cmd.Help()
			return
		}

		fromStdin := hasStdin(args)

		err = run(cmd.Context(), targets, fromStdin)
		if err != nil {
			log.Fatal(err)
		}
	},
}

func switchToPorcelain() {
	log.Tracef("Switching to porcelain")
	*porcelain = true
	*plain = true
	log.SetLevel(log.ErrorLevel)
}

func preRunAutocomplete() error {
	return preRunWithGen(true)
}

func findRoot() (string, error) {
	root, err := filepath.Abs(".")
	if err != nil {
		return "", err
	}

	parts := strings.Split(root, string(filepath.Separator))
	for len(parts) > 0 {
		p := "/" + filepath.Join(parts...)

		if _, err := os.Stat(filepath.Join(p, ".hephconfig")); err == nil {
			return p, nil
		}

		parts = parts[:len(parts)-1]
	}

	return "", fmt.Errorf("root not found")
}

func engineInit() error {
	if Engine != nil {
		return nil
	}

	if cwd := os.Getenv("HEPH_CWD"); cwd != "" {
		err := os.Chdir(cwd)
		if err != nil {
			return err
		}
	}

	root, err := findRoot()
	if err != nil {
		return err
	}

	log.Tracef("Root: %v", root)

	ctx, cancel := context.WithCancel(context.Background())

	sig := make(chan os.Signal, 1)
	signal.Notify(sig, os.Interrupt, syscall.SIGTERM)

	go func() {
		defer func() {
			signal.Stop(sig)
		}()

		<-sig
		cancel()
	}()

	Engine = engine.New(root, ctx)
	Engine.Config.Profiles = *profiles
	Engine.Pool = worker.NewPool(ctx, *workers)

	err = Engine.Parse()
	if err != nil {
		return err
	}

	return nil
}

func preRunWithGen(silent bool) error {
	err := engineInit()
	if err != nil {
		return err
	}

	if *noGen {
		log.Info("Generated targets disabled")
		return nil
	}

	deps, err := Engine.ScheduleGenPass()
	if err != nil {
		return err
	}

	err = WaitPool("PreRun gen", deps, silent)
	if err != nil {
		printTargetErr(err)
		return err
	}

	return nil
}

var cleanCmd = &cobra.Command{
	Use:   "clean",
	Short: "Clean",
	PreRunE: func(cmd *cobra.Command, args []string) error {
		return engineInit()
	},
	ValidArgsFunction: func(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective) {
		err := preRunAutocomplete()
		if err != nil {
			return nil, cobra.ShellCompDirectiveError
		}

		return autocompleteTargetName(Engine.Targets, toComplete), cobra.ShellCompDirectiveNoFileComp
	},
	RunE: func(cmd *cobra.Command, args []string) error {
		if len(args) == 0 {
			return Engine.Clean(true)
		}

		err := preRunWithGen(false)
		if err != nil {
			return err
		}

		var targets []*engine.Target
		if hasStdin(args) {
			var err error
			targets, err = parseTargetsFromStdin()
			if err != nil {
				return err
			}
		} else {
			for _, arg := range args {
				tp, err := utils.TargetParse("", arg)
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

var cleanLockCmd = &cobra.Command{
	Use:   "lock",
	Short: "Clean locks",
	Args:  cobra.NoArgs,
	PreRunE: func(cmd *cobra.Command, args []string) error {
		return engineInit()
	},
	RunE: func(cmd *cobra.Command, args []string) error {
		for _, target := range Engine.Targets {
			err := Engine.CleanTargetLock(target)
			if err != nil {
				return err
			}
		}

		return nil
	},
}

func execute() error {
	if err := rootCmd.Execute(); err != nil {
		return err
	}

	return nil
}

func Execute() {
	utils.Seed()

	if err := execute(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func printTargetErr(err error) bool {
	var terr engine.TargetFailedError
	if errors.As(err, &terr) {
		fmt.Printf("%v failed: %v\n", terr.Target.FQN, err)
		logFile := terr.Target.LogFile
		if logFile != "" {
			c := exec.Command("cat", logFile)
			output, _ := c.Output()
			fmt.Fprintln(os.Stderr)
			fmt.Fprintln(os.Stderr, string(output))
			fmt.Fprintf(os.Stderr, "log file can be found at %v:\n", logFile)
		}

		return true
	}

	return false
}
