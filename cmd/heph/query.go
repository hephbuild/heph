package main

import (
	"encoding/json"
	"fmt"
	"github.com/hephbuild/heph/bootstrap"
	"github.com/hephbuild/heph/cmd/heph/search"
	"github.com/hephbuild/heph/cmd/heph/searchui"
	"github.com/hephbuild/heph/engine"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/graphprint"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/packages"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/tgt"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/sets"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/worker/poolwait"
	"github.com/spf13/cobra"
	"gopkg.in/yaml.v3"
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
var all bool
var debugTransitive bool

func init() {
	queryCmd.AddCommand(configCmd)
	queryCmd.AddCommand(codegenCmd)
	queryCmd.AddCommand(graphCmd)
	queryCmd.AddCommand(graphDotCmd)
	queryCmd.AddCommand(changesCmd)
	queryCmd.AddCommand(targetCmd)
	queryCmd.AddCommand(pkgsCmd)
	queryCmd.AddCommand(revdepsCmd)
	queryCmd.AddCommand(depsCmd)
	queryCmd.AddCommand(hashoutCmd)
	queryCmd.AddCommand(hashinCmd)
	queryCmd.AddCommand(outRootCmd)
	queryCmd.AddCommand(orderedCachesCmd)
	queryCmd.AddCommand(labelsCmd)

	// Private, for internal testing
	queryCmd.AddCommand(cacheRootCmd)
	queryCmd.AddCommand(parseTargetCmd)

	revdepsCmd.Flags().BoolVar(&transitive, "transitive", false, "Transitively")
	depsCmd.Flags().BoolVar(&transitive, "transitive", false, "Transitively")

	targetCmd.Flags().BoolVar(&spec, "spec", false, "Print spec")
	targetCmd.Flags().BoolVar(&debugTransitive, "debug-transitive", false, "Print transitive details")

	queryCmd.Flags().StringArrayVarP(&include, "include", "i", nil, "Label/Target to include")
	queryCmd.Flags().StringArrayVarP(&exclude, "exclude", "e", nil, "Label/target to exclude, takes precedence over --include")
	queryCmd.Flags().BoolVarP(&all, "all", "a", false, "Outputs private targets")

	searchCmd.Flags().BoolVarP(&all, "all", "a", false, "Outputs private targets")

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

		bs, err := engineInit(ctx, func(bootstrap.BaseBootstrap) error {
			return bootstrap.BlockReadStdin(args)
		})
		if err != nil {
			return err
		}

		err = preRunWithGenWithOpts(ctx, PreRunOpts{
			Engine:       bs.Engine,
			PoolWaitName: "Query gen",
		})
		if err != nil {
			return err
		}

		targets := bs.Graph.Targets()
		if bootstrap.HasStdin(args) {
			tps, _, err := bootstrap.ParseTargetPathsAndArgs(args, true)
			if err != nil {
				return err
			}

			targets = graph.NewTargets(len(tps))
			for _, target := range tps {
				targets.Add(bs.Engine.Graph.Targets().Find(target.Full()))
			}
		} else {
			if !all {
				targets = targets.Public()
			}

			if len(include) == 0 && len(exclude) == 0 && !all {
				return fmt.Errorf("specify at least one of --include or --exclude")
			}
		}

		includeMatchers := make(graph.TargetMatchers, 0, len(include))
		for _, s := range include {
			includeMatchers = append(includeMatchers, graph.ParseTargetSelector("", s))
		}
		excludeMatchers := make(graph.TargetMatchers, 0, len(include))
		for _, s := range exclude {
			excludeMatchers = append(excludeMatchers, graph.ParseTargetSelector("", s))
		}

		matcher := graph.YesMatcher()
		if len(includeMatchers) > 0 {
			matcher = graph.OrMatcher(includeMatchers...)
		}
		if len(excludeMatchers) > 0 {
			matcher = graph.AndMatcher(matcher, graph.NotMatcher(graph.OrMatcher(excludeMatchers...)))
		}

		selected := make([]*tgt.Target, 0)
		for _, target := range targets.Slice() {
			if matcher(target) {
				selected = append(selected, target.Target)
			}
		}

		if len(selected) == 0 {
			return nil
		}

		fmt.Println(strings.Join(sortedTargetNames(selected, false), "\n"))
		return nil
	},
}

var searchCmd = &cobra.Command{
	Use:     "search [target]",
	Aliases: []string{"s"},
	Short:   "Search targets",
	Args:    cobra.ArbitraryArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		bs, err := engineInit(ctx, nil)
		if err != nil {
			return err
		}

		targets, _, err := preRunAutocompleteWithBootstrap(ctx, bs, all)
		if err != nil {
			return err
		}

		if len(args) == 0 {
			return searchui.TUI(targets, bs)
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

		bs, err := bootstrapInit(ctx)
		if err != nil {
			return err
		}

		b, err := yaml.Marshal(bs.Config)
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
		bs, err := preRunWithGen(cmd.Context())
		if err != nil {
			return err
		}

		paths := make([]string, 0)

		for p, t := range bs.Graph.CodegenPaths() {
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
		bs, target, err := parseTargetFromArgs(cmd.Context(), args)
		if err != nil {
			return err
		}

		ances, _, err := bs.Graph.DAG().GetAncestorsGraph(target.FQN)
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
		ctx := cmd.Context()

		bs, err := engineInit(ctx, nil)
		if err != nil {
			return err
		}

		err = preRunWithGenWithOpts(ctx, PreRunOpts{
			Engine:  bs.Engine,
			LinkAll: true,
		})
		if err != nil {
			return err
		}

		dag := bs.Graph.DAG()
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
				dag = &graph.DAG{DAG: gdag}
			case "descendants":
				gdag, _, err := dag.GetDescendantsGraph(args[1])
				if err != nil {
					return err
				}
				dag = &graph.DAG{DAG: gdag}
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
		id := func(target *graph.Target) string {
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
		bs, err := preRunWithGen(c.Context())
		if err != nil {
			return err
		}

		since := args[0]

		cmd := exec.Command("git", "--no-pager", "diff", "--name-only", since+"...HEAD")
		cmd.Dir = bs.Root.Root.Abs()
		out, err := cmd.Output()
		if err != nil {
			return err
		}

		affectedTargets := make([]*tgt.Target, 0)
		affectedFiles := strings.Split(string(out), "\n")

		allTargets := bs.Graph.Targets().Slice()

		for _, affectedFile := range affectedFiles {
		targets:
			for ti, t := range allTargets {
				for _, file := range t.HashDeps.Files {
					if strings.HasPrefix(affectedFile, file.RelRoot()) {
						log.Tracef("%v affects %v", affectedFile, t.FQN)
						affectedTargets = append(affectedTargets, t.Target)
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

		bs, target, err := parseTargetFromArgs(ctx, args)
		if err != nil {
			return err
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

		err = bs.Graph.LinkTarget(target, nil)
		if err != nil {
			return err
		}

		fmt.Println(target.FQN)
		fmt.Println()

		graphprint.Print(os.Stdout, target, debugTransitive)

		return nil
	},
}

var pkgsCmd = &cobra.Command{
	Use:   "pkgs",
	Short: "Prints pkgs details",
	Args:  cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		bs, err := preRunWithGen(ctx)
		if err != nil {
			return err
		}

		pkgs := make([]*packages.Package, 0)
		for _, p := range bs.Packages.All() {
			pkgs = append(pkgs, p)
		}
		sort.SliceStable(pkgs, func(i, j int) bool {
			return pkgs[i].Path < pkgs[j].Path
		})

		for _, p := range pkgs {
			fullname := p.Path
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
		bs, target, err := parseTargetFromArgs(cmd.Context(), args)
		if err != nil {
			return err
		}

		fn := bs.Graph.DAG().GetParents
		if transitive {
			fn = bs.Graph.DAG().GetAncestors
		}

		ancs, err := fn(target.Target)
		if err != nil {
			return err
		}

		ancestors := ads.Map(ancs, func(t *graph.Target) string {
			return t.FQN
		})

		ancestors = ads.Dedup(ancestors, func(s string) string {
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
	Short: "Prints targets that depend on the input target or file",
	Args:  cobra.ExactArgs(1),
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		bs, err := engineInit(ctx, nil)
		if err != nil {
			return err
		}

		err = preRunWithGenWithOpts(ctx, PreRunOpts{
			LinkAll: true,
			Engine:  bs.Engine,
		})
		if err != nil {
			return err
		}

		var targets []targetspec.Specer
		var fn func(target targetspec.Specer) ([]*graph.Target, error)

		tp, err := targetspec.TargetParse("", args[0])
		if err != nil {
			tperr := err

			p, err := filepath.Abs(args[0])
			if err != nil {
				return err
			}

			if !xfs.PathExists(p) {
				return fmt.Errorf("%v: is not a file and %w", args[0], tperr)
			}

			rel, err := filepath.Rel(bs.Root.Root.Abs(), p)
			if err != nil {
				return err
			}

			if strings.Contains(rel, "..") {
				return fmt.Errorf("%v is outside repo", p)
			}

			children := bs.Graph.DAG().GetFileChildren([]string{rel}, bs.Graph.Targets().Slice())
			if err != nil {
				return err
			}

			targets = targetspec.AsSpecers(children)
			fn = func(target targetspec.Specer) ([]*graph.Target, error) {
				return []*graph.Target{bs.Graph.Targets().Find(target.Spec().FQN)}, nil
			}
			if transitive {
				fn = func(target targetspec.Specer) ([]*graph.Target, error) {
					desc, err := bs.Graph.DAG().GetDescendants(target)
					desc = append(desc, bs.Graph.Targets().Find(target.Spec().FQN))
					return desc, err
				}
			}
		} else {
			target := bs.Graph.Targets().Find(tp.Full())
			if target == nil {
				return engine.NewTargetNotFoundError(tp.Full(), bs.Graph.Targets())
			}

			targets = []targetspec.Specer{target}
			fn = bs.Graph.DAG().GetChildren
			if transitive {
				fn = bs.Graph.DAG().GetDescendants
			}
		}

		revdeps := sets.NewStringSet(0)

		for _, target := range targets {
			ancs, err := fn(target)
			if err != nil {
				return err
			}
			for _, anc := range ancs {
				revdeps.Add(anc.FQN)
			}
		}

		sort.Strings(revdeps.Slice())

		for _, fqn := range revdeps.Slice() {
			fmt.Println(fqn)
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
		bs, gtarget, err := parseTargetFromArgs(ctx, args)
		if err != nil {
			return err
		}

		err = bootstrap.Run(ctx, bs.Engine, []engine.TargetRunRequest{{Target: gtarget, TargetRunRequestOpts: getRROpts()}}, getRunOpts(), false)
		if err != nil {
			return err
		}

		target := bs.Engine.Targets.Find(gtarget)

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
		bs, gtarget, err := parseTargetFromArgs(ctx, args)
		if err != nil {
			return err
		}

		err = bootstrap.Run(ctx, bs.Engine, []engine.TargetRunRequest{{Target: gtarget, TargetRunRequestOpts: getRROpts()}}, getRunOpts(), false)
		if err != nil {
			return err
		}

		target := bs.Engine.Targets.Find(gtarget)

		names := targetspec.SortOutputsForHashing(target.ActualOutFiles().Names())
		for _, name := range names {
			fmt.Println(name+":", bs.Engine.LocalCache.HashOutput(target, name))
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

		bs, gtarget, err := parseTargetFromArgs(ctx, args)
		if err != nil {
			return err
		}

		tdeps, err := bs.Engine.ScheduleTargetsWithDeps(ctx, []*graph.Target{gtarget}, []targetspec.Specer{gtarget})
		if err != nil {
			return err
		}

		err = poolwait.Wait(ctx, "Run", bs.Engine.Pool, tdeps.All(), *plain)
		if err != nil {
			return err
		}

		target := bs.Engine.Targets.Find(gtarget)

		fmt.Println(bs.Engine.LocalCache.HashInput(target))

		return nil
	},
}

var labelsCmd = &cobra.Command{
	Use:   "labels",
	Short: "Prints labels",
	Args:  cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		bs, err := preRunWithGen(cmd.Context())
		if err != nil {
			return err
		}

		labels := bs.Graph.Labels().Slice()
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

		bs, err := bootstrapInit(ctx)
		if err != nil {
			return err
		}

		fmt.Println(bs.Root.Root.Abs())

		return nil
	},
}

var orderedCachesCmd = &cobra.Command{
	Use:   "ordered-caches",
	Short: "Prints ordered caches",
	Args:  cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		bs, err := engineInit(ctx, nil)
		if err != nil {
			return err
		}

		orderedCaches, err := bs.Engine.OrderedCaches(ctx)
		if err != nil {
			return err
		}

		for _, cache := range orderedCaches {
			fmt.Println(cache.Name, cache.URI)
		}

		return nil
	},
}
