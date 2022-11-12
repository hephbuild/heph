package cmd

import (
	"context"
	"fmt"
	"github.com/bep/debounce"
	"github.com/fsnotify/fsnotify"
	log "github.com/sirupsen/logrus"
	"github.com/spf13/cobra"
	"heph/engine"
	"os"
	"path/filepath"
	"time"
)

type watchRun struct {
	ctx   context.Context
	files []string
}

var watchCmd = &cobra.Command{
	Use:               "watch",
	Aliases:           []string{"w"},
	Short:             "Watch targets",
	SilenceUsage:      true,
	SilenceErrors:     true,
	Args:              cobra.ArbitraryArgs,
	ValidArgsFunction: ValidArgsFunctionTargets,
	RunE: func(cmd *cobra.Command, args []string) error {
		ctx := cmd.Context()

		if hasStdin(args) {
			tps, err := parseTargetPathsFromStdin()
			if err != nil {
				return err
			}

			if len(tps) == 0 {
				return nil
			}
		} else {
			if len(args) == 0 {
				return nil
			}
		}

		err := preRunWithGen(cmd.Context(), false)
		if err != nil {
			return err
		}

		rrs, err := parseTargetsAndArgs(args)
		if err != nil {
			return err
		}

		if !hasStdin(args) && len(rrs) == 0 {
			_ = cmd.Help()
			return nil
		}

		fromStdin := hasStdin(args)

		targets := engine.NewTargets(len(rrs))
		for _, inv := range rrs {
			targets.Add(inv.Target)
		}

		// TODO reload files on BUILD change
		allTargets, err := Engine.DAG().GetOrderedAncestors(targets.Slice(), true)
		if err != nil {
			return err
		}

		watcher, err := fsnotify.NewWatcher()
		if err != nil {
			log.Fatal(err)
		}
		defer watcher.Close()

		files := Engine.GetFileDeps(allTargets...)
		for _, file := range files {
			err := watcher.Add(file.Abs())
			if err != nil {
				return err
			}
		}

		var runCancel context.CancelFunc
		sigsCh := make(chan watchRun)
		errCh := make(chan error)

		debounced := debounce.New(time.Second)

		eventFiles := make([]string, 0)

		go func() {
			for {
				select {
				case event, ok := <-watcher.Events:
					if !ok {
						errCh <- nil
						return
					}

					log.Debug(event)

					rel, err := filepath.Rel(Engine.Root.Abs(), event.Name)
					if err != nil {
						errCh <- err
						return
					}

					eventFiles = append(eventFiles, rel)
					currentEventFiles := eventFiles

					debounced(func() {
						if runCancel != nil {
							runCancel()
						}
						rctx, cancel := context.WithCancel(ctx)
						sigsCh <- watchRun{
							ctx:   rctx,
							files: currentEventFiles,
						}
						runCancel = cancel
					})
				case err, ok := <-watcher.Errors:
					if !ok {
						errCh <- nil
						return
					}

					errCh <- err
					return
				case <-ctx.Done():
					errCh <- ctx.Err()
					return
				}
			}
		}()

		go func() {
			sigsCh <- watchRun{ctx: ctx}
		}()

	loop:
		for {
			select {
			case r := <-sigsCh:
				fmt.Fprintln(os.Stderr)
				log.Infof("Got change...")

				localRRs := rrs
				if r.files != nil {
					allTargets, err := Engine.DAG().GetOrderedAncestors(targets.Slice(), true)
					if err != nil {
						return err
					}

					fileDescendantsTargets, err := Engine.GetFileDescendants(r.files, allTargets)
					if err != nil {
						return err
					}

					descendants, err := Engine.DAG().GetOrderedDescendants(fileDescendantsTargets, true)
					if err != nil {
						return err
					}

					localRRs = make([]engine.TargetRunRequest, 0)
					for _, target := range descendants {
						Engine.ResetCacheHashInput(target)

						if target.Gen {
							Engine.RanGenPass = false
							Engine.DisableNamedCache = false
						} else {
							if targets.Find(target.FQN) != nil {
								localRRs = append(localRRs, engine.TargetRunRequest{Target: target})
							}
						}
					}

					if !Engine.RanGenPass {
						wg, err := Engine.ScheduleGenPass(r.ctx)
						if err != nil {
							log.Error(err)
							continue loop
						}

						select {
						case <-r.ctx.Done():
							log.Error(r.ctx.Err())
							continue loop
						case <-wg.Done():
							if err := wg.Err(); err != nil {
								log.Error(err)
								continue loop
							}
						}
					}
				}

				err = run(r.ctx, localRRs, !fromStdin)
				if err != nil {
					if !printTargetError(err) {
						log.Error(err)
						continue loop
					}
				}

				log.Info("Completed successfully")

				// Allow first run to use named cache, subsequent ones will skip them
				Engine.DisableNamedCache = true
			case err := <-errCh:
				return err
			}
		}
	},
}
