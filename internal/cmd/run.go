package cmd

import (
	"context"
	"errors"
	"fmt"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/hephbuild/heph/plugin/tref"
	"github.com/spf13/cobra"
)

func init() {
	var shell boolStr
	var force bool

	var runCmd = &cobra.Command{
		Use:     "run",
		Aliases: []string{"r"},
		Args:    cobraArgs(),
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

				matcher, matcherRef, err := parseMatcherResolve(ctx, e, rs, args, cwd, root)
				if err != nil {
					return err
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
				defer func() {
					for _, res := range res {
						res.Unlock(ctx)
					}
				}()

				// TODO how to render res natively without exec
				err = execFunc(func(args hbbtexec.RunArgs) error {
					if _, ok := matcher.GetItem().(*pluginv1.TargetMatcher_Ref); ok && len(res) == 1 {
						for _, output := range res[0].Artifacts {
							fmt.Println(output.Name)
							fmt.Println("  group:    ", output.Group)
							fmt.Println("  content:  ", output.Content)
							fmt.Println("  type:     ", output.Type.String())
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

	runCmd.Flags().AddFlag(NewBoolStrFlag(&shell, "shell", "", "shell into target"))
	runCmd.Flags().BoolVarP(&force, "force", "", false, "force running")

	rootCmd.AddCommand(runCmd)
}
