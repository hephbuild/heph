//nolint:forbidigo
package cmd

import (
	"context"
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
		Args:    cobra.RangeArgs(1, 2),
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

			matcher, err := parseMatcher(args, cwd, root)
			if err != nil {
				return err
			}

			err = newTermui(ctx, func(ctx context.Context, execFunc func(f hbbtexec.ExecFunc) error) error {
				e, err := newEngine(ctx, root)
				if err != nil {
					return err
				}

				if refm, ok := matcher.Item.(*pluginv1.TargetMatcher_Ref); ok {
					ref := refm.Ref

					var shellTarget, forceTarget *pluginv1.TargetRef

					if shell.bool {
						if shell.str != "" {
							shellTarget, err = tref.Parse(shell.str)
							if err != nil {
								return fmt.Errorf("shell flag: %w", err)
							}
						}

						if shellTarget == nil {
							shellTarget = ref
						}
					}

					if force {
						forceTarget = ref
					}

					res, err := e.ResultFromRef(ctx, ref, []string{engine.AllOutputs}, engine.ResultOptions{
						InteractiveExec: func(ctx context.Context, iargs engine.InteractiveExecOptions) error {
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
						},
						Shell:       shellTarget,
						Force:       forceTarget,
						Interactive: ref,
					}, engine.GlobalResolveCache)
					if err != nil {
						return err
					}
					defer res.Unlock(ctx)

					// TODO how to render res natively without exec
					err = execFunc(func(args hbbtexec.RunArgs) error {
						for _, output := range res.Artifacts {
							fmt.Println(output.Name)
							fmt.Println("  group:    ", output.Group)
							fmt.Println("  uri:      ", output.Uri)
							fmt.Println("  type:     ", output.Type.String())
							fmt.Println("  encoding: ", output.Encoding.String())
						}

						return nil
					})
					if err != nil {
						return err
					}
				} else {
					res, err := e.ResultFromMatcher(ctx, matcher, engine.ResultOptions{}, engine.GlobalResolveCache)
					if err != nil {
						return err
					}
					defer func() {
						for _, res := range res {
							res.Unlock(ctx)
						}
					}()

					err = execFunc(func(args hbbtexec.RunArgs) error {
						fmt.Println("matched", len(res))

						return nil
					})
					if err != nil {
						return err
					}
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
