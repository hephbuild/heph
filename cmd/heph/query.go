package main

import (
	"encoding/json"
	"fmt"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/hephbuild/heph/bootstrap"
	"github.com/hephbuild/heph/cmd/heph/search"
	"github.com/hephbuild/heph/cmd/heph/searchui2"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/graphdot"
	"github.com/hephbuild/heph/graphprint"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/targetrun"
	"github.com/hephbuild/heph/utils/xfs"
	"github.com/hephbuild/heph/worker/poolwait"
	"github.com/spf13/cobra"
	"gopkg.in/yaml.v3"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
)

var include []string
var exclude []string
var spec bool
var transitive bool
var all bool
var debugTransitive bool
var filter string

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
	revdepsCmd.Flags().StringVar(&filter, "filter", "", "Filter resulting targets")
	depsCmd.Flags().StringVar(&filter, "filter", "", "Filter resulting targets")
	queryCmd.Flags().StringVar(&filter, "filter", "", "Filter resulting targets")

	targetCmd.Flags().BoolVar(&spec, "spec", false, "Print spec")
	targetCmd.Flags().BoolVar(&debugTransitive, "debug-transitive", false, "Print transitive details")

	queryCmd.Flags().StringArrayVarP(&include, "include", "i", nil, "Label/Target to include")
	queryCmd.Flags().StringArrayVarP(&exclude, "exclude", "e", nil, "Label/target to exclude, takes precedence over --include")
	queryCmd.Flags().BoolVarP(&all, "all", "a", false, "Outputs private targets")

	searchCmd.Flags().BoolVarP(&all, "all", "a", false, "Outputs private targets")

	queryCmd.RegisterFlagCompletionFunc("include", ValidArgsFunctionLabelsOrTargets)
	queryCmd.RegisterFlagCompletionFunc("exclude", ValidArgsFunctionLabelsOrTargets)

	queryCmd.Flags().MarkHidden("include")
	queryCmd.Flags().MarkHidden("exclude")
}

var queryCmd = &cobra.Command{
	Use:     "query",
	Aliases: []string{"q"},
	Short:   "Query the graph",
	Args:    cobra.RangeArgs(0, 1),
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		matcher, err := specs.MatcherFromIncludeExclude("", include, exclude)
		if err != nil {
			return err
		}

		if matcher == specs.AllMatcher {
			inputExpr := ""
			if !bootstrap.HasStdin(args) && len(args) >= 1 {
				inputExpr = args[0]
			} else if filter != "" {
				inputExpr = filter
			}

			if inputExpr != "" {
				m, err := specs.ParseMatcher(inputExpr)
				if err != nil {
					return err
				}

				matcher = m
			} else {
				if !all {
					return fmt.Errorf("you must specify a query, or -a")
				}
			}
		} else {
			log.Warnf("--include and --exclude are deprecated, instead use `heph query '%v'`", matcher.String())
		}

		bs, err := schedulerInit(ctx, func(bootstrap.BaseBootstrap) error {
			return bootstrap.BlockReadStdin(args)
		})
		if err != nil {
			return err
		}

		err = preRunWithGenWithOpts(ctx, PreRunOpts{
			Scheduler:    bs.Scheduler,
			PoolWaitName: "Query gen",
		})
		if err != nil {
			return err
		}

		targets := bs.Graph.Targets()
		if bootstrap.HasStdin(args) {
			m, _, err := bootstrap.ParseTargetAddrsAndArgs(args, true)
			if err != nil {
				return err
			}

			targets, err = bs.Graph.Targets().Filter(m)
			if err != nil {
				return err
			}
		} else {
			if !all {
				targets = bs.Graph.Targets().Public()
			}
		}

		selected, err := targets.Filter(matcher)
		if err != nil {
			return err
		}

		if selected.Len() == 0 {
			return nil
		}

		fmt.Println(strings.Join(sortedTargetNames(selected.Slice(), false), "\n"))
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

		bs, err := schedulerInit(ctx, nil)
		if err != nil {
			return err
		}

		targets, _, err := preRunAutocompleteWithBootstrap(ctx, bs, all)
		if err != nil {
			return err
		}

		if len(args) == 0 {
			p := tea.NewProgram(searchui2.New(targets, bs))

			m, err := p.Run()
			if err != nil {
				return err
			}

			if t := m.(searchui2.Model).RunTarget(); t != nil {
				t := bs.Graph.Targets().FindT(t)

				rrs, err := generateRRs(ctx, bs.Scheduler, t.AddrStruct(), nil)
				if err != nil {
					return err
				}

				err = bootstrap.Run(ctx, bs.Scheduler, rrs, getRunOpts(), true)
				if err != nil {
					return err
				}
			}

			return nil
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
			paths = append(paths, fmt.Sprintf("%v: %v", p, t.Addr))
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
		ctx := cmd.Context()

		bs, err := schedulerInit(ctx, nil)
		if err != nil {
			return err
		}

		err = preRunWithGenWithOpts(ctx, PreRunOpts{
			Scheduler: bs.Scheduler,
			LinkAll:   true,
		})
		if err != nil {
			return err
		}

		rrs, err := parseTargetsAndArgsWithScheduler(ctx, bs.Scheduler, args, false, true)
		if err != nil {
			return err
		}

		id := rrs[0].Target.Addr

		ances, _, err := bs.Graph.DAG().GetAncestorsGraph(id)
		if err != nil {
			return err
		}

		desc, _, err := bs.Graph.DAG().GetDescendantsGraph(id)
		if err != nil {
			return err
		}

		fmt.Println("Ancestors:")
		fmt.Print(ances.String())
		fmt.Println("Descendants:")
		fmt.Print(desc.String())

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

		bs, err := schedulerInit(ctx, nil)
		if err != nil {
			return err
		}

		err = preRunWithGenWithOpts(ctx, PreRunOpts{
			Scheduler: bs.Scheduler,
			LinkAll:   true,
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

		graphdot.Print(dag, false)

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

		affectedTargets := make([]*graph.Target, 0)
		affectedFiles := strings.Split(string(out), "\n")

		allTargets := bs.Graph.Targets().Slice()

		for _, affectedFile := range affectedFiles {
		targets:
			for ti, t := range allTargets {
				for _, file := range t.HashDeps.Files {
					if strings.HasPrefix(affectedFile, file.RelRoot()) {
						log.Tracef("%v affects %v", affectedFile, t.Addr)
						affectedTargets = append(affectedTargets, t)
						allTargets = append(allTargets[:ti], allTargets[ti+1:]...)
						continue targets
					}
				}
			}
		}

		for _, t := range affectedTargets {
			fmt.Println(t.Addr)
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
			if err := enc.Encode(target.Spec()); err != nil {
				return err
			}

			return nil
		}

		err = bs.Graph.LinkTarget(target, nil)
		if err != nil {
			return err
		}

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

		pkgs := bs.Packages.All()
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

		deps := graph.NewTargetsFrom(ancs)

		if filter != "" {
			m, err := specs.ParseMatcher(filter)
			if err != nil {
				return err
			}

			deps, err = deps.Filter(m)
			if err != nil {
				return err
			}
		}

		deps.Sort()

		for _, t := range deps.Slice() {
			fmt.Println(t.Addr)
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

		bs, err := schedulerInit(ctx, nil)
		if err != nil {
			return err
		}

		err = preRunWithGenWithOpts(ctx, PreRunOpts{
			LinkAll:   true,
			Scheduler: bs.Scheduler,
		})
		if err != nil {
			return err
		}

		var targets []specs.Specer
		var fn func(target specs.Specer) ([]*graph.Target, error)

		tp, err := specs.ParseTargetAddr("", args[0])
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

			targets = specs.AsSpecers(children)
			fn = func(target specs.Specer) ([]*graph.Target, error) {
				return []*graph.Target{bs.Graph.Targets().FindT(target)}, nil
			}
			if transitive {
				fn = func(target specs.Specer) ([]*graph.Target, error) {
					desc, err := bs.Graph.DAG().GetDescendants(target)
					desc = append(desc, bs.Graph.Targets().FindT(target))
					return desc, err
				}
			}
		} else {
			target := bs.Graph.Targets().Find(tp.Full())
			if target == nil {
				return specs.NewTargetNotFoundError(tp.Full(), bs.Graph.Targets())
			}

			targets = []specs.Specer{target}
			fn = bs.Graph.DAG().GetChildren
			if transitive {
				fn = bs.Graph.DAG().GetDescendants
			}
		}

		revdeps := graph.NewTargets(0)

		for _, target := range targets {
			ancs, err := fn(target)
			if err != nil {
				return err
			}

			revdeps.AddAll(ancs)
		}

		if filter != "" {
			m, err := specs.ParseMatcher(filter)
			if err != nil {
				return err
			}

			revdeps, err = revdeps.Filter(m)
			if err != nil {
				return err
			}
		}

		revdeps.Sort()

		for _, t := range revdeps.Slice() {
			fmt.Println(t.Addr)
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

		err = bootstrap.Run(ctx, bs.Scheduler, []targetrun.Request{{Target: gtarget, RequestOpts: getRROpts()}}, getRunOpts(), false)
		if err != nil {
			return err
		}

		target := bs.Scheduler.LocalCache.Metas.Find(gtarget)

		fmt.Println(filepath.Dir(target.OutExpansionRoot().Abs()))

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
		tp, err := specs.TargetOutputParse("", args[0])
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

		err = bootstrap.Run(ctx, bs.Scheduler, []targetrun.Request{{Target: gtarget, RequestOpts: getRROpts()}}, getRunOpts(), false)
		if err != nil {
			return err
		}

		target := bs.Scheduler.LocalCache.Metas.Find(gtarget)

		names := specs.SortOutputsForHashing(target.ActualOutFiles().Names())
		for _, name := range names {
			h, err := bs.Scheduler.LocalCache.HashOutput(target, name)
			if err != nil {
				return err
			}
			fmt.Println(name+":", h)
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

		bs, target, err := parseTargetFromArgs(ctx, args)
		if err != nil {
			return err
		}

		tdeps, err := bs.Scheduler.ScheduleTargetsWithDeps(ctx, []*graph.Target{target}, []specs.Specer{target})
		if err != nil {
			return err
		}

		err = poolwait.Wait(ctx, "Run", bs.Scheduler.Pool, tdeps.All(), *plain)
		if err != nil {
			return err
		}

		h, err := bs.Scheduler.LocalCache.HashInput(target)
		if err != nil {
			return err
		}
		fmt.Println(h)

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

		bs, err := schedulerInit(ctx, nil)
		if err != nil {
			return err
		}

		orderedCaches, err := bs.Scheduler.RemoteCache.OrderedCaches(ctx)
		if err != nil {
			return err
		}

		for _, cache := range orderedCaches {
			fmt.Println(cache.Name, cache.URI)
		}

		return nil
	},
}
