package main

import (
	"encoding/json"
	"fmt"
	"github.com/spf13/cobra"
	"gopkg.in/yaml.v3"
	"heph/cmd/heph/search"
	"heph/engine"
	log "heph/hlog"
	"heph/packages"
	"heph/targetspec"
	"heph/utils"
	"heph/utils/sets"
	"os"
	"os/exec"
	"path/filepath"
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
var all bool

func init() {
	queryCmd.AddCommand(configCmd)
	queryCmd.AddCommand(codegenCmd)
	queryCmd.AddCommand(fzfCmd)
	queryCmd.AddCommand(graphCmd)
	queryCmd.AddCommand(graphDotCmd)
	queryCmd.AddCommand(changesCmd)
	queryCmd.AddCommand(targetCmd)
	queryCmd.AddCommand(pkgsCmd)
	queryCmd.AddCommand(revdepsCmd)
	queryCmd.AddCommand(depsCmd)
	queryCmd.AddCommand(outCmd)
	queryCmd.AddCommand(hashoutCmd)
	queryCmd.AddCommand(hashinCmd)
	queryCmd.AddCommand(outRootCmd)
	queryCmd.AddCommand(labelsCmd)

	// Private, for internal testing
	queryCmd.AddCommand(cacheRootCmd)
	queryCmd.AddCommand(parseTargetCmd)

	revdepsCmd.Flags().BoolVar(&transitive, "transitive", false, "Transitively")
	depsCmd.Flags().BoolVar(&transitive, "transitive", false, "Transitively")

	outCmd.Flags().StringVar(&output, "output", "", "Output name")

	targetCmd.Flags().BoolVar(&spec, "spec", false, "Print spec")

	queryCmd.Flags().StringArrayVarP(&include, "include", "i", nil, "Label/Target to include")
	queryCmd.Flags().StringArrayVarP(&exclude, "exclude", "e", nil, "Label/target to exclude, takes precedence over --include")
	queryCmd.Flags().BoolVarP(&all, "all", "a", false, "Outputs private targets")
	fzfCmd.Flags().BoolVarP(&all, "all", "a", false, "Outputs private targets")

	queryCmd.RegisterFlagCompletionFunc("include", ValidArgsFunctionLabelsOrTargets)
	queryCmd.RegisterFlagCompletionFunc("exclude", ValidArgsFunctionLabelsOrTargets)
}

var queryCmd = &cobra.Command{
	Use:     "query",
	Aliases: []string{"q"},
	Short:   "Query the graph",
	Args:    cobra.RangeArgs(0, 1),
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		err := blockReadStdin(args)
		if err != nil {
			return err
		}

		err = engineInit(ctx)
		if err != nil {
			return err
		}

		err = preRunWithGenWithOpts(ctx, PreRunOpts{
			Engine:       Engine,
			PoolWaitName: "Query gen",
		})
		if err != nil {
			return err
		}

		targets := Engine.Targets.Slice()
		if hasStdin(args) {
			var err error
			targets, err = parseTargetsFromStdin(Engine)
			if err != nil {
				return err
			}
		} else {
			if !all {
				targets = engine.FilterPublicTargets(targets)
			}

			if len(include) == 0 && len(exclude) == 0 && !all {
				return fmt.Errorf("specify at least one of --include or --exclude")
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

		matcher := engine.YesMatcher()
		if len(includeMatchers) > 0 {
			matcher = engine.OrMatcher(includeMatchers...)
		}
		if len(excludeMatchers) > 0 {
			matcher = engine.AndMatcher(matcher, engine.NotMatcher(engine.OrMatcher(excludeMatchers...)))
		}

		selected := make([]*engine.Target, 0)
		for _, target := range targets {
			if matcher(target) {
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

var fzfCmd = &cobra.Command{
	Use:   "fzf",
	Short: "Fuzzy search targets",
	Args:  cobra.RangeArgs(0, 1),
	RunE: func(cmd *cobra.Command, args []string) error {
		log.Warnf("Deprecated, use heph search")

		targets, _, err := preRunAutocompleteInteractive(cmd.Context(), all, false)
		if err != nil {
			return err
		}

		if len(args) == 0 {
			return search.TUI(targets)
		}

		return search.Search(targets, args[0])
	},
}

var searchCmd = &cobra.Command{
	Use:     "search [target]",
	Aliases: []string{"s"},
	Short:   "Search targets",
	Args:    cobra.ArbitraryArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		targets, _, err := preRunAutocompleteInteractive(cmd.Context(), all, false)
		if err != nil {
			return err
		}

		if len(args) == 0 {
			return search.TUI(targets)
		}

		return search.Search(targets, strings.Join(args, " "))
	},
}

var configCmd = &cobra.Command{
	Use:   "config",
	Short: "Prints config",
	Args:  cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		err := engineInit(ctx)
		if err != nil {
			return err
		}

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
	RunE: func(cmd *cobra.Command, args []string) error {
		err := preRunWithGen(cmd.Context())
		if err != nil {
			return err
		}

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
	Use:               "graph <target>",
	Short:             "Prints deps target graph",
	Args:              cobra.ExactArgs(1),
	ValidArgsFunction: ValidArgsFunctionTargets,
	RunE: func(cmd *cobra.Command, args []string) error {
		target, err := parseTargetFromArgs(cmd.Context(), args)
		if err != nil {
			return err
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
	Use:               "graphdot [ancestors|descendants <target>]",
	Short:             "Outputs graph do",
	Args:              cobra.ArbitraryArgs,
	ValidArgsFunction: ValidArgsFunctionTargets,
	RunE: func(cmd *cobra.Command, args []string) error {
		err := engineInit(cmd.Context())
		if err != nil {
			return err
		}

		err = preRunWithGenWithOpts(cmd.Context(), PreRunOpts{
			Engine:  Engine,
			LinkAll: true,
		})
		if err != nil {
			return err
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

			skip := sets.NewStringSet(0)
			//for _, tool := range target.Tools.Targets {
			//	skip.Add(tool.Target.FQN)
			//}

			for _, ancestor := range parents {
				if skip.Has(ancestor.FQN) {
					continue
				}

				fmt.Printf("    %v -> %v;\n", id(ancestor), id(target))
			}
			fmt.Println()
		}

		fmt.Println("}")

		return nil
	},
}

var changesCmd = &cobra.Command{
	Use:               "changes <since>",
	Short:             "Prints deps target changes",
	Args:              cobra.ExactArgs(1),
	ValidArgsFunction: ValidArgsFunctionTargets,
	RunE: func(c *cobra.Command, args []string) error {
		err := preRunWithGen(c.Context())
		if err != nil {
			return err
		}

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
	Use:               "target <target>",
	Short:             "Prints target details",
	Args:              cobra.ExactArgs(1),
	ValidArgsFunction: ValidArgsFunctionTargets,
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		tp, err := targetspec.TargetParse("", args[0])
		if err != nil {
			return err
		}

		err = preRunWithGen(ctx)
		if err != nil {
			return err
		}

		target := Engine.Targets.Find(tp.Full())
		if target == nil {
			return engine.NewTargetNotFoundError(tp.Full())
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

		err = Engine.LinkTarget(target, nil)
		if err != nil {
			return err
		}

		fmt.Println(target.FQN)

		fmt.Println("Transitive:")
		printTools("    ", target.OwnTransitive.Tools)
		printDeps("    ", target.OwnTransitive.Deps)

		fmt.Println("Deep Transitive:")
		printTools("    ", target.DeepOwnTransitive.Tools)
		printDeps("    ", target.DeepOwnTransitive.Deps)

		fmt.Println("Deps:")
		printTools("    ", target.Tools)
		printDeps("    ", target.Deps)

		fmt.Println("Deps from transitive:")
		printTools("    ", target.TransitiveDeps.Tools)
		printDeps("    ", target.TransitiveDeps.Deps)

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
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		err := preRunWithGen(ctx)
		if err != nil {
			return err
		}

		pkgs := make([]*packages.Package, 0)
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
			fmt.Printf("  path: %v\n", p.Root.RelRoot())
			fmt.Println()
		}
		return nil
	},
}

var depsCmd = &cobra.Command{
	Use:   "deps <target>",
	Short: "Prints target dependencies",
	Args:  cobra.ExactArgs(1),
	RunE: func(cmd *cobra.Command, args []string) error {
		target, err := parseTargetFromArgs(cmd.Context(), args)
		if err != nil {
			return err
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

var revdepsCmd = &cobra.Command{
	Use:   "revdeps <target>",
	Short: "Prints targets dependent on the input",
	Args:  cobra.ExactArgs(1),
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		err := preRunWithGenWithOpts(ctx, PreRunOpts{
			LinkAll: true,
		})
		if err != nil {
			return err
		}

		tp, err := targetspec.TargetParse("", args[0])
		if err != nil {
			return err
		}

		target := Engine.Targets.Find(tp.Full())
		if target == nil {
			return engine.NewTargetNotFoundError(tp.Full())
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

func printTargetOutput(target *engine.Target, output string) error {
	paths := target.ActualOutFiles().All()
	if output != "" {
		if !target.ActualOutFiles().HasName(output) {
			return fmt.Errorf("%v: output `%v` does not exist", target.FQN, output)
		}
		paths = target.ActualOutFiles().Name(output)
	} else if len(paths) == 0 {
		return fmt.Errorf("%v: does not output anything", target.FQN)
	}

	for _, path := range paths {
		fmt.Println(path.Abs())
	}
	return nil
}

var outCmd = &cobra.Command{
	Use:               "out <target>",
	Short:             "Prints targets output path",
	Args:              cobra.ExactArgs(1),
	ValidArgsFunction: ValidArgsFunctionTargets,
	Hidden:            true,
	RunE: func(cmd *cobra.Command, args []string) error {
		log.Warnf("Deprecated, use heph run xxx --print-out")

		ctx := cmd.Context()

		target, err := parseTargetFromArgs(ctx, args)
		if err != nil {
			return err
		}

		err = run(ctx, Engine, []engine.TargetRunRequest{{Target: target, NoCache: *nocache}}, false)
		if err != nil {
			return err
		}

		err = printTargetOutput(target, output)
		if err != nil {
			return err
		}

		return nil
	},
}

var cacheRootCmd = &cobra.Command{
	Use:               "cacheroot <target>",
	Short:             "Prints targets cache root",
	Hidden:            true,
	Args:              cobra.ExactArgs(1),
	ValidArgsFunction: ValidArgsFunctionTargets,
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()
		target, err := parseTargetFromArgs(ctx, args)
		if err != nil {
			return err
		}

		err = run(ctx, Engine, []engine.TargetRunRequest{{Target: target, NoCache: *nocache}}, false)
		if err != nil {
			return err
		}

		fmt.Println(filepath.Dir(target.OutExpansionRoot.Abs()))

		return nil
	},
}

var parseTargetCmd = &cobra.Command{
	Use:               "parsetarget <target>",
	Short:             "Prints parsed target",
	Hidden:            true,
	Args:              cobra.ExactArgs(1),
	ValidArgsFunction: ValidArgsFunctionTargets,
	RunE: func(cmd *cobra.Command, args []string) error {
		tp, err := targetspec.TargetOutputParse("", args[0])
		if err != nil {
			return err
		}

		enc := json.NewEncoder(os.Stdout)
		enc.SetIndent("", "    ")
		err = enc.Encode(tp)
		if err != nil {
			return err
		}

		return nil
	},
}

var hashoutCmd = &cobra.Command{
	Use:               "hashout <target>",
	Short:             "Prints targets output hash",
	Args:              cobra.ExactArgs(1),
	ValidArgsFunction: ValidArgsFunctionTargets,
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()
		target, err := parseTargetFromArgs(ctx, args)
		if err != nil {
			return err
		}

		err = run(ctx, Engine, []engine.TargetRunRequest{{Target: target, NoCache: *nocache}}, false)
		if err != nil {
			return err
		}

		names := targetspec.SortOutputsForHashing(target.ActualOutFiles().Names())
		for _, name := range names {
			fmt.Println(name+":", Engine.HashOutput(target, name))
		}

		return nil
	},
}

var hashinCmd = &cobra.Command{
	Use:               "hashin <target>",
	Short:             "Prints targets input hash",
	Args:              cobra.ExactArgs(1),
	ValidArgsFunction: ValidArgsFunctionTargets,
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		target, err := parseTargetFromArgs(ctx, args)
		if err != nil {
			return err
		}

		tdeps, err := Engine.ScheduleTargetsWithDeps(ctx, []*engine.Target{target}, target)
		if err != nil {
			return err
		}

		err = WaitPool("Run", Engine.Pool, tdeps.All())
		if err != nil {
			return err
		}

		fmt.Println(Engine.HashInput(target))

		return nil
	},
}

var labelsCmd = &cobra.Command{
	Use:   "labels",
	Short: "Prints labels",
	Args:  cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		err := preRunWithGen(cmd.Context())
		if err != nil {
			return err
		}

		labels := Engine.Labels.Slice()
		sort.Strings(labels)

		for _, label := range labels {
			fmt.Println(label)
		}

		return nil
	},
}

var outRootCmd = &cobra.Command{
	Use:   "root",
	Short: "Prints repo root",
	Args:  cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		err := engineInit(ctx)
		if err != nil {
			return err
		}

		fmt.Println(Engine.Root.Abs())

		return nil
	},
}
