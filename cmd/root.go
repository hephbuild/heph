package cmd

import (
	"fmt"
	"github.com/mattn/go-isatty"
	log "github.com/sirupsen/logrus"
	"github.com/spf13/cobra"
	"heph/engine"
	"heph/utils"
	"math/rand"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"time"
)

var isTerm bool

var logLevel *string
var profiles *[]string
var plain *bool
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
	profiles = rootCmd.PersistentFlags().StringArray("profile", nil, "config profiles")
	porcelain = rootCmd.PersistentFlags().Bool("porcelain", false, "Machine readable output, disables all logging")

	plain = rootCmd.PersistentFlags().Bool("plain", false, "Plain output")
	workers = rootCmd.PersistentFlags().Int("workers", runtime.NumCPU(), "Number of workers")

	rootCmd.Flags().SetInterspersed(false)
	setupRootUsage()
}

var Engine *engine.Engine

var rootCmd = &cobra.Command{
	Use:           "heph",
	Short:         "Efficient build system",
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

		err := preRun()
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
	PreRunE: func(cmd *cobra.Command, args []string) error {
		return preRunWithStaticAnalysis()
	},
	ValidArgsFunction: func(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective) {
		err := preRunAutocomplete()
		if err != nil {
			return nil, cobra.ShellCompDirectiveError
		}

		return autocompleteTargetName(Engine.Targets, toComplete), cobra.ShellCompDirectiveNoFileComp
	},
	Run: func(cmd *cobra.Command, args []string) {
		targets, err := parseTargetsAndArgs(args)
		if err != nil {
			log.Fatal(err)
		}

		if len(targets) == 0 {
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
	return preRun()
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

func preRun() error {
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

	Engine = engine.New(root)
	Engine.Config.Profiles = *profiles

	err = Engine.Parse()
	if err != nil {
		return err
	}

	return nil
}

func preRunWithStaticAnalysis() error {
	err := preRun()
	if err != nil {
		return err
	}

	err = Engine.RunStaticAnalysis()
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
		return preRun()
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

		var targets []*engine.Target
		if hasStdin(args) {
			var err error
			targets, err = parseTargetsFromStdin()
			if err != nil {
				return nil
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
		return preRun()
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
	rand.Seed(int64(time.Now().Nanosecond()))

	if err := execute(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func printTargetErr(err error) bool {
	if err, ok := err.(engine.TargetFailedError); ok {
		fmt.Printf("%v failed: %v\n", err.Target.FQN, err)
		logFile := err.Target.LogFile
		if logFile != "" {
			c := exec.Command("tail", "-n", "10", logFile)
			output, _ := c.Output()
			fmt.Println()
			fmt.Println(string(output))
			fmt.Printf("log file can be found at %v:\n", logFile)
		}

		return true
	}

	return false
}
