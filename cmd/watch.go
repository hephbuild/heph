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
	"sync"
	"time"
)

type watchRun struct {
	ctx               context.Context
	events            []watchEvent
	needRunBuildFiles bool
}

type watchEvent struct {
	fsnotify.Event
	RelPath string
}

type watchCtx struct {
	ctx       context.Context
	watcher   *fsnotify.Watcher
	e         *engine.Engine
	runCancel context.CancelFunc
	sigsCh    chan watchRun
	rrs       []engine.TargetRunRequest
	targets   *engine.Targets

	m           sync.Mutex
	graphSigsCh chan struct{}
}

func (w *watchCtx) run(r watchRun, fromStdin bool) error {
	w.m.Lock()
	defer w.m.Unlock()

	fmt.Fprintln(os.Stderr)
	log.Infof("Got change...")

	localRRs := w.rrs
	if r.events != nil {
		allTargets, err := w.e.DAG().GetOrderedAncestors(w.targets.Slice(), true)
		if err != nil {
			return err
		}

		files := make([]string, 0)
		for _, event := range r.events {
			files = append(files, event.RelPath)
		}

		fileDescendantsTargets, err := w.e.GetFileDescendants(files, allTargets)
		if err != nil {
			return err
		}

		descendants, err := w.e.DAG().GetOrderedDescendants(fileDescendantsTargets, true)
		if err != nil {
			return err
		}

		localRRs = make([]engine.TargetRunRequest, 0)
		for _, target := range descendants {
			w.e.ResetCacheHashInput(target)

			if target.Gen {
				w.e.RanGenPass = false
				w.e.DisableNamedCache = false
			} else {
				if w.targets.Find(target.FQN) != nil {
					localRRs = append(localRRs, engine.TargetRunRequest{Target: target})
				}
			}
		}

		if !w.e.RanGenPass {
			wg, err := w.e.ScheduleGenPass(r.ctx)
			if err != nil {
				log.Error(err)
				return nil
			}

			select {
			case <-r.ctx.Done():
				log.Error(r.ctx.Err())
				return nil
			case <-wg.Done():
				if err := wg.Err(); err != nil {
					log.Error(err)
					return nil
				}
			}
		}
	}

	err := run(r.ctx, w.e, localRRs, !fromStdin)
	if err != nil {
		if !printTargetError(err) {
			log.Error(err)
			return nil
		}
	}

	log.Info("Completed successfully")

	// Allow first run to use named cache, subsequent ones will skip them
	w.e.DisableNamedCache = true

	return nil
}

func (w *watchCtx) triggerRun(currentEvents []watchEvent) {
	if w.runCancel != nil {
		w.runCancel()
	}

	// Should be nil if currentEvents is nil
	var filteredEvents []watchEvent
	if currentEvents != nil {
		m := map[string]watchEvent{}

		for _, e := range currentEvents {
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

			filteredEvents = append(filteredEvents, e)
		}
	}

	//for _, event := range filteredEvents {
	//	log.Warn(event)
	//}

	for _, e := range filteredEvents {
		if e.Op.Has(fsnotify.Create) || e.Op.Has(fsnotify.Remove) || e.Op.Has(fsnotify.Rename) {
			// Reload graph
			w.graphSigsCh <- struct{}{}

			return
		}
	}

	rctx, cancel := context.WithCancel(w.ctx)
	w.sigsCh <- watchRun{
		ctx:    rctx,
		events: filteredEvents,
	}
	w.runCancel = cancel
}

func (w *watchCtx) runGraph(args []string) error {
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

	err = preRunWithGenWithEngine(w.ctx, e, false)
	if err != nil {
		return err
	}

	w.rrs, err = parseTargetsAndArgs(e, args)
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

	w.triggerRun(nil)

	return nil
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

			if w.runCancel != nil {
				w.runCancel()
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
				w.triggerRun(currentEvents)
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
				err := wctx.run(r, fromStdin)
				if err != nil {
					return err
				}
			case err := <-errCh:
				return err
			}
		}
	},
}
