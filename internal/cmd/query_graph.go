package cmd

import (
	"context"
	"fmt"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	"github.com/hephbuild/heph/internal/hdag"
	"github.com/hephbuild/heph/internal/tmatch"
	"github.com/hephbuild/heph/lib/tref"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/spf13/cobra"
)

func init() {
	var scope string

	cmd := &cobra.Command{
		Use:  "graph [descendants|ancestors] target",
		Args: cobra.RangeArgs(1, 2),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx := cmd.Context()

			ctx, stop := newSignalNotifyContext(ctx)
			defer stop()

			cwd, err := engine.Cwd()
			if err != nil {
				return err
			}

			root, err := engine.Root()
			if err != nil {
				return err
			}

			var ref *pluginv1.TargetRef
			dagType := engine.DAGTypeAll
			if len(args) == 1 {
				ref, err = parseTargetRef(args[0], cwd, root)
				if err != nil {
					return err
				}
			} else {
				switch args[0] {
				case "dependees":
					dagType = engine.DAGTypeDescendants
				case "deps":
					dagType = engine.DAGTypeAncestors
				default:
					return fmt.Errorf("invalid graph type: %v", args[0])
				}

				ref, err = parseTargetRef(args[1], cwd, root)
				if err != nil {
					return err
				}
			}

			scopeMatcher := tmatch.All()
			if scope != "" {
				scopeMatcher, err = tmatch.ParsePackageMatcher(scope, cwd, root)
				if err != nil {
					return err
				}
			}

			err = newTermui(ctx, func(ctx context.Context, execFunc func(f hbbtexec.ExecFunc) error) error {
				e, err := newEngine(ctx, root)
				if err != nil {
					return err
				}

				rs, cleanRs := e.NewRequestState()
				defer cleanRs()

				dag, err := e.DAG(ctx, rs, ref, dagType, scopeMatcher)
				if err != nil {
					return err
				}

				return execFunc(func(args hbbtexec.RunArgs) error {
					return hdag.Dot(
						args.Stdout, dag,
						hdag.WithVertexRenderer(func(v *pluginv1.TargetRef) string {
							if path, ok := tref.ParseFile(v); ok {
								return path
							}

							return tref.Format(v)
						}),
						hdag.WithVertexExtra(func(v *pluginv1.TargetRef) string {
							if tref.Equal(v, ref) {
								return `style="filled", fillcolor="lightgreen"`
							}

							if _, ok := tref.ParseFile(v); ok {
								return `style="filled", fillcolor="lightgrey"`
							}

							return ""
						}),
						hdag.WithEdgeRenderer(func(src, dst *pluginv1.TargetRef, meta string) string {
							if meta == "dep" {
								return ""
							}

							return meta
						}),
						hdag.WithEdgeFilter(func(src, dst *pluginv1.TargetRef, meta string) bool {
							if meta == "resolve" {
								return false
							}

							return true
						}),
					)
				})
			})
			if err != nil {
				return err
			}

			return nil
		},
	}

	cmd.Flags().StringVar(&scope, "scope", "", "Filter universe of targets")

	queryCmd.AddCommand(cmd)
}
