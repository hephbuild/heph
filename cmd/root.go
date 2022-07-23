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
	"path/filepath"
	"runtime"
	"strings"
	"time"
)

var isTerm bool

var logLevel *string
var profiles *[]string
var plain *bool
var quiet *bool
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

	rootCmd.AddCommand(runCmd)
	rootCmd.AddCommand(cleanCmd)
	rootCmd.AddCommand(queryCmd)

	logLevel = rootCmd.PersistentFlags().String("log_level", log.InfoLevel.String(), "log level")
	profiles = rootCmd.PersistentFlags().StringArray("profile", nil, "config profiles")
	quiet = rootCmd.PersistentFlags().Bool("quiet", false, "Quiet")

	plain = rootCmd.PersistentFlags().Bool("plain", false, "Plain output")
	workers = rootCmd.PersistentFlags().Int("workers", runtime.NumCPU()/2, "Number of workers")

	rootCmd.Flags().SetInterspersed(false)
}

var Engine *engine.Engine

var rootCmd = &cobra.Command{
	Use:           "heph",
	Short:         "Efficient build system",
	SilenceUsage:  true,
	SilenceErrors: true,
}

var runCmd = &cobra.Command{
	Use:           "run",
	Short:         "Run target",
	SilenceUsage:  true,
	SilenceErrors: true,
	Args:          cobra.ArbitraryArgs,
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
	Run: func(cmd *cobra.Command, args []string) {
		targets, err := parseTargetsAndArgs(args)
		if err != nil {
			log.Fatal(err)
		}

		if len(targets) == 0 {
			err := cmd.Help()
			if err != nil {
				log.Fatal(err)
			}
			return
		}

		fromStdin := hasStdin(args)

		err = run(cmd.Context(), targets, fromStdin)
		if err != nil {
			log.Fatal(err)
		}
	},
}

func preRunAutocomplete() error {
	return preRun()
}

func findRoot() (string, error) {
	if root := os.Getenv("HEPH_ROOT"); root != "" {
		return root, nil
	}

	root, err := filepath.Abs(".")
	if err != nil {
		return "", err
	}

	parts := strings.Split(root, string(filepath.Separator))
	for len(parts) > 0 {
		p := "/" + filepath.Join(parts...)

		if _, err := os.Stat(filepath.Join(p, ".hephconfig")); !os.IsNotExist(err) {
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

	lvl, err := log.ParseLevel(*logLevel)
	if err != nil {
		return err
	}

	log.SetLevel(lvl)

	if *quiet {
		log.SetLevel(log.WarnLevel)
	}

	root, err := findRoot()
	if err != nil {
		return err
	}

	Engine = engine.New(root)
	Engine.Config.Profiles = *profiles

	err = Engine.Parse()
	if err != nil {
		return err
	}

	return nil
}

var cleanCmd = &cobra.Command{
	Use:   "clean",
	Short: "Clean",
	PersistentPreRunE: func(cmd *cobra.Command, args []string) error {
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
