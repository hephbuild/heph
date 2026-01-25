package cmd

import (
	"context"
	"errors"
	"fmt"
	"io"

	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hcobra"
	"github.com/hephbuild/heph/internal/hio"
	"github.com/hephbuild/heph/internal/tmatch"
	"github.com/hephbuild/heph/lib/tref"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"github.com/spf13/cobra"
)

func init() {
	var shell hcobra.BoolStr
	var force bool
	var ignore []string
	var listOut bool
	var listArtifacts bool
	var catOut bool
	var hashOut bool

	cmdArgs := parseTargetMatcherArgs{cmdName: "run"}

	cmd := &cobra.Command{
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

				if shell.Bool && shell.Str != "" {
					rs.Shell, err = tref.Parse(shell.Str)
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

				if shell.Bool && rs.Shell == nil {
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

				err = execFunc(func(args hbbtexec.RunArgs) error {
					switch {
					case listOut:
						for _, re := range res {
							for _, output := range re.Artifacts {
								if output.GetType() != pluginv1.Artifact_TYPE_OUTPUT {
									continue
								}

								for f, err := range hartifact.FilesReader(ctx, output.Artifact) {
									if err != nil {
										return err
									}

									_ = f.Close()

									fmt.Println(f.Path)
								}
							}
						}
					case catOut:
						stdout := &hio.SpyLast{Writer: args.Stdout}
						for _, re := range res {
							for _, output := range re.Artifacts {
								if output.GetType() != pluginv1.Artifact_TYPE_OUTPUT {
									continue
								}

								for f, err := range hartifact.FilesReader(ctx, output.Artifact) {
									if err != nil {
										return err
									}

									if stdout.Written && stdout.LastByte != '\n' {
										fmt.Println() // new line
									}
									stdout.Reset()

									_, err = io.Copy(stdout, f)
									if err != nil {
										return err
									}

									_ = f.Close()
								}
							}
						}
					case listArtifacts:
						for _, re := range res {
							for _, output := range re.Artifacts {
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
						}
					case hashOut:
						for _, re := range res {
							for _, output := range re.Artifacts {
								if output.GetName() == "" {
									fmt.Println(output.Hashout)
								} else {
									fmt.Println(output.GetGroup(), output.Hashout)
								}
							}
						}
					default:
						if !matcher.HasRef() {
							fmt.Println("matched", len(res))
						}
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

	runFlagGroup := hcobra.NewFlagSet("Flags")
	runFlagGroup.StringArrayVar(&ignore, "ignore", nil, "Filter universe of targets")
	runFlagGroup.BoolStrVar(&shell, "shell", "", "shell into target")
	runFlagGroup.BoolVarP(&force, "force", "", false, "force running")
	hcobra.AddLocalFlagSet(cmd, runFlagGroup)

	outFlagGroup := hcobra.NewFlagSet("Output Flags")
	outFlagGroup.BoolVarP(&listArtifacts, "list-artifacts", "", false, "List output artifacts")
	outFlagGroup.BoolVarP(&listOut, "list-out", "", false, "List output paths")
	outFlagGroup.BoolVarP(&catOut, "cat-out", "", false, "Print outputs to stdout")
	outFlagGroup.BoolVarP(&hashOut, "hashout", "", false, "Print output hashes to stdout")
	hcobra.AddLocalFlagSet(cmd, outFlagGroup)

	rootCmd.AddCommand(cmd)
}
