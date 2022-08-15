package cmd

import (
	"context"
	"encoding/json"
	"fmt"
	log "github.com/sirupsen/logrus"
	"github.com/spf13/cobra"
	"gopkg.in/yaml.v3"
	"heph/engine"
	"heph/utils"
	"heph/worker"
	"os"
	"os/exec"
	"sort"
	"strconv"
	"strings"
)

var include []string
var exclude []string
var spec bool

func init() {
	queryCmd.AddCommand(configCmd)
	queryCmd.AddCommand(codegenCmd)
	queryCmd.AddCommand(alltargetsCmd)
	queryCmd.AddCommand(graphCmd)
	queryCmd.AddCommand(graphDotCmd)
	queryCmd.AddCommand(changesCmd)
	queryCmd.AddCommand(targetCmd)

	targetCmd.Flags().BoolVar(&spec, "spec", false, "Print spec")

	queryCmd.Flags().StringArrayVar(&include, "include", nil, "Labels to include")
	queryCmd.Flags().StringArrayVar(&exclude, "exclude", nil, "Labels to exclude, takes precedence over --include")

	autocompleteTargetOrLabel := func(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective) {
		err := preRunAutocomplete()
		if err != nil {
			return nil, cobra.ShellCompDirectiveError
		}

		suggestions := autocompleteTargetName(Engine.Targets, toComplete)
		suggestions = append(suggestions, autocompleteLabel(toComplete)...)

		return suggestions, cobra.ShellCompDirectiveNoFileComp
	}

	queryCmd.RegisterFlagCompletionFunc("include", autocompleteTargetOrLabel)
	queryCmd.RegisterFlagCompletionFunc("exclude", autocompleteTargetOrLabel)
}

var queryCmd = &cobra.Command{
	Use:   "query",
	Short: "Filter targets",
	Args:  cobra.ArbitraryArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		err := engineInit()
		if err != nil {
			return err
		}

		ctx, cancel := context.WithCancel(cmd.Context())
		defer cancel()

		pool := worker.NewPool(ctx, *workers)
		defer pool.Stop()

		if !*noGen {
			err = Engine.ScheduleStaticAnalysis(ctx, pool)
			if err != nil {
				return err
			}

			if isTerm && !*plain {
				err := DynamicRenderer("Query Static Analysis", ctx, cancel, pool)
				if err != nil {
					return fmt.Errorf("dynamic renderer: %w", err)
				}
			}
			<-pool.Done()

			if err := pool.Err; err != nil {
				printTargetErr(err)
				return err
			}
		}

		targets := Engine.Targets
		if hasStdin(args) {
			var err error
			targets, err = parseTargetsFromStdin()
			if err != nil {
				return err
			}
		}

		includeMatchers := make(engine.TargetMatchers, 0)
		for _, s := range include {
			includeMatchers = append(includeMatchers, engine.ParseTargetSelector("", s))
		}
		excludeMatchers := make(engine.TargetMatchers, 0)
		for _, s := range exclude {
			excludeMatchers = append(excludeMatchers, engine.ParseTargetSelector("", s))
		}

		selected := make([]*engine.Target, 0)

		for _, target := range targets {
			if (len(include) == 0 || includeMatchers.AnyMatch(target)) &&
				!excludeMatchers.AnyMatch(target) {
				selected = append(selected, target)
			}
		}

		if len(selected) == 0 {
			return nil
		}

		fmt.Println(strings.Join(sortedTargetNames(selected, false), "\n"))
		return nil
	},
}

var alltargetsCmd = &cobra.Command{
	Use:   "alltargets",
	Short: "Prints all targets",
	Args:  cobra.NoArgs,
	PreRunE: func(cmd *cobra.Command, args []string) error {
		return preRunWithStaticAnalysis(false)
	},
	RunE: func(cmd *cobra.Command, args []string) error {
		for _, target := range sortedTargets(Engine.Targets, false) {
			fmt.Println(target.FQN)
		}

		return nil
	},
}

var configCmd = &cobra.Command{
	Use:   "config",
	Short: "Prints config",
	Args:  cobra.NoArgs,
	PreRunE: func(cmd *cobra.Command, args []string) error {
		return engineInit()
	},
	RunE: func(cmd *cobra.Command, args []string) error {
		b, err := yaml.Marshal(Engine.Config)
		if err != nil {
			return err
		}

		fmt.Println(string(b))

		return nil
	},
}

var codegenCmd = &cobra.Command{
	Use:   "codegen",
	Short: "Prints codegen paths",
	Args:  cobra.NoArgs,
	PreRunE: func(cmd *cobra.Command, args []string) error {
		return engineInit()
	},
	RunE: func(cmd *cobra.Command, args []string) error {
		paths := make([]string, 0)

		for p, t := range Engine.CodegenPaths() {
			paths = append(paths, fmt.Sprintf("%v: %v", p, t.FQN))
		}

		sort.Strings(paths)

		for _, s := range paths {
			fmt.Println(s)
		}

		return nil
	},
}

var graphCmd = &cobra.Command{
	Use:   "graph <target>",
	Short: "Prints deps target graph",
	Args:  cobra.ExactValidArgs(1),
	PreRunE: func(cmd *cobra.Command, args []string) error {
		return preRunWithStaticAnalysis(false)
	},
	ValidArgsFunction: func(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective) {
		err := preRunAutocomplete()
		if err != nil {
			return nil, cobra.ShellCompDirectiveError
		}

		return autocompleteTargetName(Engine.Targets, toComplete), cobra.ShellCompDirectiveNoFileComp
	},
	RunE: func(cmd *cobra.Command, args []string) error {
		tp, err := utils.TargetParse("", args[0])
		if err != nil {
			return err
		}

		target := Engine.Targets.Find(tp.Full())
		if target == nil {
			return fmt.Errorf("target %v not found", args[0])
		}

		ances, _, err := Engine.DAG().GetAncestorsGraph(target.FQN)
		if err != nil {
			return err
		}

		fmt.Print(ances.String())

		return nil
	},
}

var graphDotCmd = &cobra.Command{
	Use:   "graphdot",
	Short: "Outputs graph do",
	Args:  cobra.ArbitraryArgs,
	PreRunE: func(cmd *cobra.Command, args []string) error {
		return preRunWithStaticAnalysis(false)
	},
	ValidArgsFunction: func(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective) {
		err := preRunAutocomplete()
		if err != nil {
			return nil, cobra.ShellCompDirectiveError
		}

		return autocompleteTargetName(Engine.Targets, toComplete), cobra.ShellCompDirectiveNoFileComp
	},
	RunE: func(cmd *cobra.Command, args []string) error {
		fmt.Printf(`
digraph G  {
	fontname="Helvetica,Arial,sans-serif"
	node [fontname="Helvetica,Arial,sans-serif"]
	edge [fontname="Helvetica,Arial,sans-serif"]
	rankdir="LR"
	node [fontsize=10, shape=box, height=0.25]
	edge [fontsize=10]
`)
		id := func(target *engine.Target) string {
			return strconv.Quote(target.FQN)
		}

		targets := Engine.DAG().GetVertices()

		if hasStdin(args) {
			var err error
			targets, err = parseTargetsFromStdin()
			if err != nil {
				return nil
			}
		}

		Engine.DAG().DFSWalk(engine.Walker(func(target *engine.Target) {
			if targets.Find(target.FQN) == nil {
				return
			}

			parents, err := Engine.DAG().GetParents(target)
			if err != nil {
				panic(err)
			}
			extra := ""
			if target.IsGroup() {
				//extra = ` color="red"`
				return
			}

			fmt.Printf("    %v [label=\"%v\"%v];\n", id(target), target.FQN, extra)

			for _, ancestor := range parents {
				fmt.Printf("    %v -> %v;\n", id(ancestor), id(target))
			}
			fmt.Println()
		}))

		fmt.Println("}")

		return nil
	},
}

var changesCmd = &cobra.Command{
	Use:   "changes <since>",
	Short: "Prints deps target changes",
	Args:  cobra.ExactValidArgs(1),
	PreRunE: func(cmd *cobra.Command, args []string) error {
		return preRunWithStaticAnalysis(false)
	},
	ValidArgsFunction: func(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective) {
		err := preRunAutocomplete()
		if err != nil {
			return nil, cobra.ShellCompDirectiveError
		}

		return autocompleteTargetName(Engine.Targets, toComplete), cobra.ShellCompDirectiveNoFileComp
	},
	RunE: func(_ *cobra.Command, args []string) error {
		since := args[0]

		cmd := exec.Command("git", "--no-pager", "diff", "--name-only", since+"...HEAD")
		cmd.Dir = Engine.Root
		out, err := cmd.Output()
		if err != nil {
			return err
		}

		affectedTargets := make(engine.Targets, 0)
		affectedFiles := strings.Split(string(out), "\n")

		allTargets := Engine.Targets

		for _, affectedFile := range affectedFiles {
		targets:
			for ti, t := range allTargets {
				for _, file := range t.HashDeps.Files {
					if strings.HasPrefix(affectedFile, file.RelRoot()) {
						log.Tracef("%v affects %v", affectedFile, t.FQN)
						affectedTargets = append(affectedTargets, t)
						allTargets = append(allTargets[:ti], allTargets[ti+1:]...)
						continue targets
					}
				}
			}
		}

		for _, t := range affectedTargets {
			fmt.Println(t.FQN)
		}

		return nil
	},
}

var targetCmd = &cobra.Command{
	Use:   "target <target>",
	Short: "Prints target details",
	Args:  cobra.ExactValidArgs(1),
	PreRunE: func(cmd *cobra.Command, args []string) error {
		if spec {
			return engineInit()
		} else {
			return preRunWithStaticAnalysis(false)
		}
	},
	ValidArgsFunction: func(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective) {
		err := preRunAutocomplete()
		if err != nil {
			return nil, cobra.ShellCompDirectiveError
		}

		return autocompleteTargetName(Engine.Targets, toComplete), cobra.ShellCompDirectiveNoFileComp
	},
	RunE: func(_ *cobra.Command, args []string) error {
		tp, err := utils.TargetParse("", args[0])
		if err != nil {
			return err
		}

		target := Engine.Targets.Find(tp.Full())
		if target == nil {
			return engine.TargetNotFoundError(tp.Full())
		}

		if spec {
			enc := json.NewEncoder(os.Stdout)
			enc.SetEscapeHTML(false)
			enc.SetIndent("", "    ")
			if err := enc.Encode(target.TargetSpec); err != nil {
				return err
			}

			return nil
		}

		fmt.Println(target.FQN)
		fmt.Println("Deps - Targets")
		for _, t := range target.Deps.Targets {
			fmt.Printf("  %v\n", t.FQN)
		}
		fmt.Println("Deps - Files")
		for _, t := range target.Deps.Files {
			fmt.Printf("  %v\n", t.RelRoot())
		}

		return nil
	},
}
