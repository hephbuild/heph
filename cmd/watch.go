package cmd

import (
	"context"
	"fmt"
	"github.com/bep/debounce"
	"github.com/bmatcuk/doublestar/v4"
	"github.com/fsnotify/fsnotify"
	"github.com/spf13/cobra"
	"heph/engine"
	log "heph/hlog"
	"heph/utils/fs"
	"os"
	"path/filepath"
	"sync"
	"time"
)

type watchRun struct {
	rrs engine.TargetRunRequests
}

type watchEvent struct {
	fsnotify.Event
	RelPath string
}

type watchCtx struct {
	ctx       context.Context
	watcher   *fsnotify.Watcher
	e         *engine.Engine
	sigsCh    chan watchRun
	runCancel context.CancelFunc
	runCh     chan struct{}
	rrs       engine.TargetRunRequests
	targets   *engine.Targets

	m           sync.Mutex
	graphSigsCh chan struct{}
}

func (w *watchCtx) cancelRunAndWait() {
	if w.runCancel != nil {
		w.runCancel()
	}

	// Wait for previous run to end
	if w.runCh != nil {
		select {
		case <-w.runCh:
		case <-time.After(time.Second):
			log.Infof("Waiting for previous run to finish...")
			<-w.runCh
		}
	}
}

func (w *watchCtx) run(r watchRun, fromStdin bool) {
	ctx, cancel := context.WithCancel(w.ctx)
	defer cancel()

	w.cancelRunAndWait()

	w.m.Lock()
	defer w.m.Unlock()

	w.runCancel = cancel

	fmt.Fprintln(os.Stderr)
	log.Infof("Got change...")

	runCh := make(chan struct{})
	w.runCh = runCh
	defer close(runCh)

	if !w.e.RanGenPass {
		wg, err := w.e.ScheduleGenPass(ctx)
		if err != nil {
			log.Error(err)
			return
		}

		select {
		case <-ctx.Done():
			log.Error(ctx.Err())
			return
		case <-wg.Done():
			if *summary || *summaryGen {
				PrintSummary(w.e.Stats, *summaryGen)
				w.e.Stats.Reset()
			}

			if err := wg.Err(); err != nil {
				log.Error(err)
				return
			}
		}
	}

	err := run(ctx, w.e, r.rrs, !fromStdin)

	if *summary || *summaryGen {
		PrintSummary(w.e.Stats, *summaryGen)
		w.e.Stats.Reset()
	}

	if err != nil {
		printHumanError(err)
		return
	}

	log.Info("Completed successfully")

	// Allow first run to use named cache, subsequent ones will skip them
	w.e.DisableNamedCacheWrite = true
}

func (w *watchCtx) triggerRun(currentEvents []watchEvent) error {
	// Should be nil if currentEvents is nil
	var filteredEvents []watchEvent
	if currentEvents != nil {
		codegenPaths := w.e.CodegenPaths()
		m := map[string]watchEvent{}

		for _, e := range currentEvents {
			if _, ok := codegenPaths[e.RelPath]; ok {
				continue
			}

			if me, ok := m[e.Name]; ok {
				me.Op |= e.Op
				m[e.Name] = me
			} else {
				m[e.Name] = e
			}
		}

		filteredEvents = make([]watchEvent, 0)
		for _, e := range m {
			if e.Op.Has(fsnotify.Create) && e.Op.Has(fsnotify.Remove) {
				continue
			}

			if e.Op.Has(fsnotify.Create) && !fs.PathExists(e.Name) {
				continue
			}

			ignored := false
			for _, p := range *ignore {
				match, err := doublestar.PathMatch(p, e.Name)
				if err != nil {
					return err
				}
				ignored = match

				if match {
					break
				}
			}
			if ignored {
				continue
			}

			filteredEvents = append(filteredEvents, e)
		}

		if len(filteredEvents) == 0 {
			return nil
		}
	}

	//for _, event := range filteredEvents {
	//	log.Warn(event)
	//}

	for _, e := range filteredEvents {
		if e.Op.Has(fsnotify.Create) || e.Op.Has(fsnotify.Remove) || e.Op.Has(fsnotify.Rename) {
			log.Warnf("event %v", e.String())
			// Reload graph
			w.graphSigsCh <- struct{}{}

			return nil
		}
	}

	localRRs := w.rrs
	if filteredEvents != nil {
		allTargets, err := w.e.DAG().GetOrderedAncestors(w.targets.Slice(), true)
		if err != nil {
			return err
		}

		files := make([]string, 0)
		for _, event := range filteredEvents {
			files = append(files, event.RelPath)
		}

		directDescendants, err := w.e.GetFileDescendants(files, allTargets)
		if err != nil {
			return err
		}

		descendants, err := w.e.DAG().GetOrderedDescendants(directDescendants, true)
		if err != nil {
			return err
		}

		for _, target := range descendants {
			w.e.ResetCacheHashInput(target)

			if target.Gen {
				w.e.RanGenPass = false
				w.e.DisableNamedCacheWrite = false
			}
		}

		localRRs = make([]engine.TargetRunRequest, 0)
		if !w.e.RanGenPass {
			// Run all if gen has not run yet
			localRRs = w.rrs
		} else {
			for _, target := range descendants {
				if w.targets.Find(target.FQN) != nil {
					localRRs = append(localRRs, w.rrs.Get(target))
				}
			}
		}

		if len(localRRs) == 0 && w.e.RanGenPass {
			return nil
		}
	}

	w.sigsCh <- watchRun{
		rrs: localRRs,
	}

	return nil
}

func (w *watchCtx) runGraph(args []string) error {
	w.cancelRunAndWait()

	w.m.Lock()
	defer w.m.Unlock()

	if w.e != nil {
		w.e.RunExitHandlers()
	}

	var err error
	w.e, err = engineFactory()
	if err != nil {
		return err
	}
	e := w.e

	w.rrs, err = parseTargetsAndArgsWithEngine(w.ctx, e, args)
	if err != nil {
		return err
	}

	if !hasStdin(args) && len(w.rrs) == 0 {
		return nil
	}

	w.targets = engine.NewTargets(len(w.rrs))
	for _, inv := range w.rrs {
		w.targets.Add(inv.Target)
	}

	allTargets, err := e.DAG().GetOrderedAncestors(w.targets.Slice(), true)
	if err != nil {
		return err
	}

	files := e.GetFileHashDeps(allTargets...)
	for _, path := range e.GetWatcherList(files) {
		err := w.watcher.Add(path)
		if err != nil {
			return err
		}
	}

	return w.triggerRun(nil)
}

func (w *watchCtx) watchGraph(args []string) error {
	defer func() {
		if w.e != nil {
			w.e.RunExitHandlers()
		}
	}()

	for {
		select {
		case <-w.graphSigsCh:
			log.Warnf("Reload graph")

			for _, p := range w.watcher.WatchList() {
				_ = w.watcher.Remove(p)
			}

			err := w.runGraph(args)
			if err != nil {
				return err
			}
		case <-w.ctx.Done():
			return w.ctx.Err()
		}
	}
}

func (w *watchCtx) watchFiles() error {
	debounced := debounce.New(500 * time.Millisecond)
	eventsAccumulator := make([]watchEvent, 0)

	for {
		select {
		case event, ok := <-w.watcher.Events:
			if !ok {
				return nil
			}

			// Ignore if its only a chmod
			if event.Op == fsnotify.Chmod {
				continue
			}

			rel, err := filepath.Rel(w.e.Root.Abs(), event.Name)
			if err != nil {
				return err
			}

			eventsAccumulator = append(eventsAccumulator, watchEvent{
				Event:   event,
				RelPath: rel,
			})
			currentEvents := eventsAccumulator

			debounced(func() {
				err = w.triggerRun(currentEvents)
				if err != nil {
					log.Error(err)
				}
				eventsAccumulator = nil
			})
		case err, ok := <-w.watcher.Errors:
			if !ok {
				return nil
			}

			return err
		case <-w.ctx.Done():
			return w.ctx.Err()
		}
	}
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

		fromStdin := hasStdin(args)

		watcher, err := fsnotify.NewWatcher()
		if err != nil {
			return err
		}
		defer watcher.Close()

		errCh := make(chan error)

		wctx := &watchCtx{
			ctx:         ctx,
			watcher:     watcher,
			sigsCh:      make(chan watchRun),
			graphSigsCh: make(chan struct{}),
		}

		go func() {
			errCh <- wctx.watchGraph(args)
		}()
		go func() {
			errCh <- wctx.watchFiles()
		}()

		go func() {
			// Kickstart the whole thing
			wctx.graphSigsCh <- struct{}{}
		}()

		for {
			select {
			case r := <-wctx.sigsCh:
				go wctx.run(r, fromStdin)
			case err := <-errCh:
				return err
			}
		}
	},
}
