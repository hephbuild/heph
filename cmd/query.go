package cmd

import (
	"encoding/json"
	"fmt"
	log "github.com/sirupsen/logrus"
	"github.com/spf13/cobra"
	"gopkg.in/yaml.v3"
	"heph/engine"
	"heph/utils"
	"os"
	"os/exec"
	"sort"
	"strconv"
	"strings"
	"time"
)

var include []string
var exclude []string
var spec bool
var transitive bool
var output string

func init() {
	queryCmd.AddCommand(configCmd)
	queryCmd.AddCommand(codegenCmd)
	queryCmd.AddCommand(alltargetsCmd)
	queryCmd.AddCommand(graphCmd)
	queryCmd.AddCommand(graphDotCmd)
	queryCmd.AddCommand(changesCmd)
	queryCmd.AddCommand(targetCmd)
	queryCmd.AddCommand(pkgsCmd)
	queryCmd.AddCommand(depsOnCmd)
	queryCmd.AddCommand(depsCmd)
	queryCmd.AddCommand(outCmd)

	depsOnCmd.Flags().BoolVar(&transitive, "transitive", false, "Transitively")
	depsCmd.Flags().BoolVar(&transitive, "transitive", false, "Transitively")

	outCmd.Flags().StringVar(&output, "output", "", "Output name")

	targetCmd.Flags().BoolVar(&spec, "spec", false, "Print spec")

	queryCmd.Flags().StringArrayVarP(&include, "include", "i", nil, "Label/Target to include")
	queryCmd.Flags().StringArrayVarP(&exclude, "exclude", "e", nil, "Label/target to exclude, takes precedence over --include")

	autocompleteTargetOrLabel := func(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective) {
		err := preRunAutocomplete(cmd.Context())
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

		if hasStdin(args) {
			// Block and read stdin here to prevent multiple bubbletea running at the same time
			_, err := parseTargetPathsFromStdin()
			if err != nil {
				return err
			}
		}

		if !*noGen {
			deps, err := Engine.ScheduleGenPass(cmd.Context())
			if err != nil {
				return err
			}

			err = WaitPool("Query Gen", deps, false)
			if err != nil {
				return err
			}
		}

		targets := Engine.Targets.Slice()
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
		return preRunWithGen(cmd.Context(), false)
	},
	RunE: func(cmd *cobra.Command, args []string) error {
		for _, target := range sortedTargets(Engine.Targets.Slice(), false) {
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
		return preRunWithGen(cmd.Context(), false)
	},
	ValidArgsFunction: func(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective) {
		err := preRunAutocomplete(cmd.Context())
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
	Use:   "graphdot [ancestors|descendants <target>]",
	Short: "Outputs graph do",
	Args:  cobra.ArbitraryArgs,
	PreRunE: func(cmd *cobra.Command, args []string) error {
		return preRunWithGen(cmd.Context(), false)
	},
	ValidArgsFunction: func(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective) {
		err := preRunAutocomplete(cmd.Context())
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

		dag := Engine.DAG()
		if len(args) > 0 {
			if len(args) < 2 {
				return fmt.Errorf("requires two args")
			}

			switch args[0] {
			case "ancestors":
				gdag, _, err := dag.GetAncestorsGraph(args[1])
				if err != nil {
					return err
				}
				dag = &engine.DAG{DAG: gdag}
			case "descendants":
				gdag, _, err := dag.GetDescendantsGraph(args[1])
				if err != nil {
					return err
				}
				dag = &engine.DAG{DAG: gdag}
			default:
				return fmt.Errorf("must be one of ancestors, descendants")
			}
		}

		for _, target := range dag.GetVertices() {
			extra := ""
			if target.IsGroup() {
				//extra = ` color="red"`
				continue
			}

			log.Tracef("walk %v", target.FQN)

			parentsStart := time.Now()
			parents, err := dag.GetParents(target)
			log.Debugf("parents took %v (got %v)", time.Since(parentsStart), len(parents))
			if err != nil {
				panic(err)
			}

			fmt.Printf("    %v [label=\"%v\"%v];\n", id(target), target.FQN, extra)

			for _, ancestor := range parents {
				fmt.Printf("    %v -> %v;\n", id(ancestor), id(target))
			}
			fmt.Println()
		}

		fmt.Println("}")

		return nil
	},
}

var changesCmd = &cobra.Command{
	Use:   "changes <since>",
	Short: "Prints deps target changes",
	Args:  cobra.ExactValidArgs(1),
	PreRunE: func(cmd *cobra.Command, args []string) error {
		return preRunWithGen(cmd.Context(), false)
	},
	ValidArgsFunction: func(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective) {
		err := preRunAutocomplete(cmd.Context())
		if err != nil {
			return nil, cobra.ShellCompDirectiveError
		}

		return autocompleteTargetName(Engine.Targets, toComplete), cobra.ShellCompDirectiveNoFileComp
	},
	RunE: func(_ *cobra.Command, args []string) error {
		since := args[0]

		cmd := exec.Command("git", "--no-pager", "diff", "--name-only", since+"...HEAD")
		cmd.Dir = Engine.Root.Abs()
		out, err := cmd.Output()
		if err != nil {
			return err
		}

		affectedTargets := make([]*engine.Target, 0)
		affectedFiles := strings.Split(string(out), "\n")

		allTargets := Engine.Targets.Slice()

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
	ValidArgsFunction: func(cmd *cobra.Command, args []string, toComplete string) ([]string, cobra.ShellCompDirective) {
		err := preRunAutocomplete(cmd.Context())
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

		if *noGen {
			err := engineInit()
			if err != nil {
				return err
			}
		} else {
			err := preRunWithGen(cmd.Context(), false)
			if err != nil {
				return err
			}
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

		fmt.Println("Require Transitive:")
		printTools("    ", target.RequireTransitive.Tools)
		printDeps("    ", target.RequireTransitive.Deps)

		fmt.Println("Deps:")
		printTools("    ", target.Tools)
		printDeps("    ", target.Deps)

		fmt.Println("Transitive Deps:")
		printTools("    ", target.Transitive.Tools)
		printDeps("    ", target.Transitive.Deps)

		return nil
	},
}

func printDeps(indent string, deps engine.TargetNamedDeps) {
	fmt.Println(indent + "Targets:")
	for _, t := range deps.All().Targets {
		fmt.Printf(indent+"  %v\n", t.Target.FQN)
	}
	fmt.Println(indent + "Files:")
	for _, t := range deps.All().Files {
		fmt.Printf(indent+"  %v\n", t.RelRoot())
	}
}

func printTools(indent string, tools engine.TargetTools) {
	fmt.Println(indent + "Tools:")
	for _, t := range tools.Targets {
		fmt.Printf(indent+"  %v\n", t.Target.FQN)
	}
	fmt.Println(indent + "Host tools:")
	for _, t := range tools.Hosts {
		fmt.Printf(indent+"  %v\n", t.Name)
	}
}

var pkgsCmd = &cobra.Command{
	Use:   "pkgs",
	Short: "Prints pkgs details",
	Args:  cobra.NoArgs,
	PreRunE: func(cmd *cobra.Command, args []string) error {
		if spec {
			return engineInit()
		} else {
			return preRunWithGen(cmd.Context(), false)
		}
	},
	RunE: func(_ *cobra.Command, args []string) error {
		pkgs := make([]*engine.Package, 0)
		for _, p := range Engine.Packages {
			pkgs = append(pkgs, p)
		}
		sort.SliceStable(pkgs, func(i, j int) bool {
			return pkgs[i].FullName < pkgs[j].FullName
		})

		for _, p := range pkgs {
			fullname := p.FullName
			if fullname == "" {
				fullname = "<root>"
			}

			fmt.Printf("%v\n", fullname)
			fmt.Printf("  %v\n", p.Root.RelRoot())
			fmt.Println()
		}
		return nil
	},
}

var depsCmd = &cobra.Command{
	Use:   "deps <target>",
	Short: "Prints target dependencies",
	Args:  cobra.ExactArgs(1),
	PreRunE: func(cmd *cobra.Command, args []string) error {
		return preRunWithGen(cmd.Context(), false)
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

		fn := Engine.DAG().GetParents
		if transitive {
			fn = Engine.DAG().GetAncestors
		}

		ancestors := make([]string, 0)
		ancs, err := fn(target)
		if err != nil {
			return err
		}
		for _, anc := range ancs {
			ancestors = append(ancestors, anc.FQN)
		}

		ancestors = utils.DedupKeepLast(ancestors, func(s string) string {
			return s
		})
		sort.Strings(ancestors)

		for _, fqn := range ancestors {
			fmt.Println(fqn)
		}

		return nil
	},
}

var depsOnCmd = &cobra.Command{
	Use:   "depson <target>",
	Short: "Prints targets dependent on",
	Args:  cobra.ExactArgs(1),
	PreRunE: func(cmd *cobra.Command, args []string) error {
		return preRunWithGen(cmd.Context(), false)
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

		fn := Engine.DAG().GetChildren
		if transitive {
			fn = Engine.DAG().GetDescendants
		}

		descendants := make([]string, 0)

		ancs, err := fn(target)
		if err != nil {
			return err
		}
		for _, anc := range ancs {
			descendants = append(descendants, anc.FQN)
		}

		descendants = utils.DedupKeepLast(descendants, func(s string) string {
			return s
		})
		sort.Strings(descendants)

		for _, fqn := range descendants {
			fmt.Println(fqn)
		}

		return nil
	},
}

var outCmd = &cobra.Command{
	Use:   "out <target>",
	Short: "Prints targets output path",
	Args:  cobra.ExactArgs(1),
	PreRunE: func(cmd *cobra.Command, args []string) error {
		return preRunWithGen(cmd.Context(), false)
	},
	RunE: func(cmd *cobra.Command, args []string) error {
		tp, err := utils.TargetParse("", args[0])
		if err != nil {
			return err
		}

		target := Engine.Targets.Find(tp.Full())
		if target == nil {
			return engine.TargetNotFoundError(tp.Full())
		}

		err = run(cmd.Context(), []TargetInvocation{{Target: target}}, false, false)
		if err != nil {
			return err
		}

		paths := target.ActualFilesOut()
		if output != "" {
			if !target.NamedActualFilesOut().HasName(output) {
				return fmt.Errorf("output %v does not exist", output)
			}
			paths = target.NamedActualFilesOut().Name(output)
		}

		for _, path := range paths {
			fmt.Println(path.Abs())
		}

		return nil
	},
}
