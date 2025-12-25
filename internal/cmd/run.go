package cmd

import (
	"context"
	"errors"
	"fmt"

	"github.com/hephbuild/heph/internal/tmatch"
	"github.com/hephbuild/heph/lib/tref"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/spf13/cobra"
)

func init() {
	var shell boolStr
	var force bool
	var ignore []string

	cmdArgs := parseMatcherArgs{cmdName: "run"}

	var runCmd = &cobra.Command{
		Use:               cmdArgs.Use(),
		Aliases:           []string{"r"},
		Args:              cmdArgs.Args(),
		ValidArgsFunction: cmdArgs.ValidArgsFunction(),
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

			err = newTermui(ctx, func(ctx context.Context, execFunc func(f hbbtexec.ExecFunc) error) error {
				e, err := newEngine(ctx, root)
				if err != nil {
					return err
				}

				rs, cleanRs := e.NewRequestState()
				defer cleanRs()

				rs.InteractiveExec = func(ctx context.Context, iargs engine.InteractiveExecOptions) error {
					return execFunc(func(args hbbtexec.RunArgs) error {
						if iargs.Pty {
							err := args.MakeRaw()
							if err != nil {
								return err
							}
						}

						iargs.Run(ctx, engine.ExecOptions{
							Stdin:  args.Stdin,
							Stdout: args.Stdout,
							Stderr: args.Stderr,
						})

						return nil
					})
				}

				if shell.bool && shell.str != "" {
					rs.Shell, err = tref.Parse(shell.str)
					if err != nil {
						return fmt.Errorf("shell flag: %w", err)
					}
				}

				matcher, matcherRef, err := cmdArgs.ParseResolve(ctx, e, rs, args, cwd, root)
				if err != nil {
					return err
				}

				for _, s := range ignore {
					ignoreMatcher, err := tmatch.ParsePackageMatcher(s, cwd, root)
					if err != nil {
						return err
					}

					matcher = tmatch.And(matcher, tmatch.Not(ignoreMatcher))
				}

				rs.Interactive = matcherRef

				if shell.bool && rs.Shell == nil {
					if matcherRef != nil {
						rs.Shell = matcherRef
					} else {
						return errors.New("shell only supports a single target, specify --shell=//some:target")
					}
				}

				if force {
					rs.Force = matcher
				}

				res, err := e.ResultsFromMatcher(ctx, rs, matcher)
				if err != nil {
					return err
				}
				defer res.Unlock(ctx)

				// TODO how to render res natively without exec
				err = execFunc(func(args hbbtexec.RunArgs) error {
					if matcher.HasRef() && len(res) == 1 {
						for _, output := range res[0].Artifacts {
							fmt.Println(output.GetName())
							fmt.Println("  group:    ", output.GetGroup())
							switch output.WhichContent() {
							case pluginv1.Artifact_File_case:
								fmt.Println("  content:", output.GetFile())
							case pluginv1.Artifact_Raw_case:
								fmt.Println("  content:", output.GetRaw())
							case pluginv1.Artifact_TarPath_case:
								fmt.Println("  content:", output.GetTarPath())
							case pluginv1.Artifact_TargzPath_case:
								fmt.Println("  content:", output.GetTargzPath())
							case pluginv1.Artifact_Content_not_set_case:
								fmt.Println("  content: <not set>")
							}
							fmt.Println("  type:     ", output.GetType().String())
						}
					} else {
						fmt.Println("matched", len(res))
					}

					return nil
				})
				if err != nil {
					return err
				}

				return nil
			})
			if err != nil {
				return err
			}

			return nil
		},
	}

	runCmd.Flags().StringArrayVar(&ignore, "ignore", nil, "Filter universe of targets")
	runCmd.Flags().AddFlag(NewBoolStrFlag(&shell, "shell", "", "shell into target"))
	runCmd.Flags().BoolVarP(&force, "force", "", false, "force running")

	rootCmd.AddCommand(runCmd)
}
