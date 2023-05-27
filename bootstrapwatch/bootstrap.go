package bootstrapwatch

import (
	"context"
	"errors"
	"fmt"
	"github.com/bep/debounce"
	"github.com/charmbracelet/lipgloss"
	"github.com/fsnotify/fsnotify"
	"github.com/hephbuild/heph/bootstrap"
	"github.com/hephbuild/heph/engine"
	"github.com/hephbuild/heph/engine/buildfiles"
	"github.com/hephbuild/heph/engine/graph"
	"github.com/hephbuild/heph/engine/hroot"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/sandbox"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/utils"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/fs"
	"github.com/hephbuild/heph/utils/maps"
	"github.com/hephbuild/heph/worker"
	"github.com/hephbuild/heph/worker/poolui"
	"os"
	"path/filepath"
	"sync"
	"time"
)

type State struct {
	root  *hroot.State
	tps   []targetspec.TargetPath
	targs []string

	ctx      context.Context
	watcher  *fsnotify.Watcher
	ignore   []string
	close    func()
	rrs      engine.TargetRunRequests
	bootopts bootstrap.BootOpts
	runopts  bootstrap.RunOpts
	rropts   engine.TargetRunRequestOpts
	cbs      bootstrap.EngineBootstrap
	pool     *worker.Pool
	sigCh    chan sigEvent

	triggeredHashed maps.Map[string, string]
}

func status(s string) {
	_, _ = fmt.Fprintln(log.Writer(), successStyle.Render("WCH|", s))
}

func (s *State) Start() error {
	go func() {
		err := s.watchFiles()
		if err != nil && !errors.Is(err, context.Canceled) {
			log.Error("watchFiles:", err)
		}
	}()

	err := s.watchSigs()
	if err != nil && !errors.Is(err, context.Canceled) {
		return err
	}

	return nil
}

func (s *State) Close() {
	s.close()
}

type fsEvent struct {
	fsnotify.Event
	RelPath string
	At      time.Time
}

type sigEvent struct {
	bs     bootstrap.EngineBootstrap
	rrs    engine.TargetRunRequests
	events []fsEvent
}

func Boot(ctx context.Context, root *hroot.State, opts bootstrap.BootOpts, cliopts bootstrap.RunOpts, rropts engine.TargetRunRequestOpts, tps []targetspec.TargetPath, targs []string, ignore []string) (*State, error) {
	watcher, err := fsnotify.NewWatcher()
	if err != nil {
		return nil, err
	}

	pool := worker.NewPool(opts.Workers)
	opts.Pool = pool

	sigCh := make(chan sigEvent)

	s := &State{
		ignore:   ignore,
		root:     root,
		ctx:      ctx,
		runopts:  cliopts,
		rropts:   rropts,
		bootopts: opts,
		watcher:  watcher,
		pool:     pool,
		tps:      tps,
		targs:    targs,
		sigCh:    sigCh,
		close: func() {
			err := watcher.Close()
			if err != nil {
				log.Error("watcher close:", err)
			}
			close(sigCh)
			pool.Stop(nil)
		},
	}

	return s, nil
}

func (s *State) watchFiles() error {
	root := s.root.Root.Abs()

	debounced := debounce.New(500 * time.Millisecond)
	var cc CancellableCtx
	var m sync.Mutex
	eventsAccumulator := make([]fsEvent, 0, 10)

	eventsCh := make(chan fsnotify.Event, 1000)
	defer close(eventsCh)
	triggerCh := make(chan []fsEvent, 0)
	defer close(triggerCh)

	go func() {
		for rawEvents := range triggerCh {
			rawEvents := rawEvents
			events := s.cleanEvents(rawEvents)

			if events != nil && len(events) == 0 {
				continue
			}

			ctx := cc.CancelAndNew(s.ctx)

			go func() {
				defer cc.Done()

				m.Lock()
				defer m.Unlock()

				err := s.trigger(ctx, events)
				if err != nil {
					log.Error(err)
					return
				}

				eventsAccumulator = ads.Filter(eventsAccumulator, func(event fsEvent) bool {
					return !ads.Contains(rawEvents, event)
				})
			}()
		}
	}()

	go func() {
		for event := range eventsCh {
			rel, err := filepath.Rel(root, event.Name)
			if err != nil {
				log.Error(event.Name, err)
				continue
			}

			var at time.Time
			info, err := os.Lstat(event.Name)
			if err == nil {
				at = info.ModTime()
			}

			m.Lock()
			eventsAccumulator = append(eventsAccumulator, fsEvent{
				Event:   event,
				RelPath: rel,
				At:      at,
			})
			m.Unlock()

			debounced(func() {
				triggerCh <- eventsAccumulator
			})
		}
	}()

	// Kickstart watch
	triggerCh <- nil

	for {
		select {
		case event, ok := <-s.watcher.Events:
			if !ok {
				return nil
			}

			log.Debug("EVENT", event)
			eventsCh <- event
		case err, ok := <-s.watcher.Errors:
			if !ok {
				return nil
			}

			return err
		case <-s.ctx.Done():
			return s.ctx.Err()
		}
	}
}

func (s *State) cleanEvents(events []fsEvent) []fsEvent {
	if events == nil {
		return nil
	}

	m := make(map[string]fsEvent, len(events)/2)
	for _, e := range events {
		// Ignore if it's only a chmod
		if e.Op == fsnotify.Chmod {
			continue
		}

		if me, ok := m[e.Name]; ok {
			me.Op |= e.Op
			if e.At.After(me.At) {
				me.At = e.At
			}
			m[e.Name] = me
		} else {
			m[e.Name] = e
		}
	}

	filteredEvents := make([]fsEvent, 0)
	for _, e := range m {
		if e.Op.Has(fsnotify.Create) && e.Op.Has(fsnotify.Remove) {
			e.Op ^= fsnotify.Create
			e.Op ^= fsnotify.Remove
			e.Op |= fsnotify.Write
		}

		if e.Op.Has(fsnotify.Create) && !fs.PathExists(e.Name) {
			continue
		}

		filteredEvents = append(filteredEvents, e)
	}

	if bs := s.cbs; bs.Graph != nil {
		return s.cleanEventsWithBootstrap(bs, filteredEvents)
	}

	return filteredEvents
}

func (s *State) cleanEventsWithBootstrap(bs bootstrap.EngineBootstrap, ogevents []fsEvent) []fsEvent {
	events := make([]fsEvent, 0, len(ogevents))
	for _, e := range ogevents {
		match, err := utils.PathMatchAny(e.RelPath, append(s.ignore, bs.Config.Watch.Ignore...)...)
		if err != nil {
			log.Error(e.RelPath, err)
		}
		if match {
			continue
		}

		// Ignore codegen changes
		if _, ok := bs.Graph.GetCodegenOrigin(e.RelPath); ok {
			continue
		}

		events = append(events, e)
	}

	return events
}

func (s *State) trigger(ctx context.Context, events []fsEvent) error {
	bs := s.cbs

	//for _, event := range events {
	//	log.Warn(event)
	//}

	for _, e := range events {
		if e.Op.Has(fsnotify.Create) || e.Op.Has(fsnotify.Remove) || e.Op.Has(fsnotify.Rename) {
			log.Debug("New Engine: C/REM/REN", e)
			bs = bootstrap.EngineBootstrap{}
			break
		}

		if ok, _ := utils.PathMatchAny(e.RelPath, buildfiles.BuildFilesPattern); ok {
			log.Debug("New Engine: BUILD", e)
			bs = bootstrap.EngineBootstrap{}
			break
		}
	}

	if bs.Engine == nil {
		cbs, err := bootstrap.BootWithEngine(ctx, s.bootopts)
		if err != nil {
			return fmt.Errorf("boot: %w", err)
		}
		bs = cbs
	}

	// TODO: interrupt console only if gen needs running
	disconnectConsole()
	defer connectConsole()

	status("Figuring out if anything changed...")
	printEvents(events)

	rrs, err := bootstrap.GenerateRRs(ctx, bs.Engine, s.tps, s.targs, s.rropts, s.runopts.Plain)
	if err != nil {
		return err
	}

	for _, rr := range rrs {
		ancestors, err := bs.Graph.DAG().GetOrderedAncestors([]*graph.Target{rr.Target}, true)
		if err != nil {
			return err
		}
		for _, ancestor := range ancestors {
			bs.Engine.LocalCache.ResetCacheHashInput(ancestor)
		}
	}

	// Run the rrs's deps, excluding the rrs's themselves
	tdepsMap, err := bs.Engine.ScheduleTargetRRsWithDeps(ctx, rrs, targetspec.AsSpecers(rrs.Targets().Slice()))
	if err != nil {
		return err
	}

	tdeps := tdepsMap.All()

	err = poolui.Wait("Change", bs.Pool, tdeps, s.runopts.Plain)
	if err != nil {
		return err
	}

	var filteredRRs engine.TargetRunRequests
	if s.cbs.Engine == nil {
		filteredRRs = rrs
	} else {
		for _, rr := range rrs {
			currHash := s.triggeredHashed.Get(rr.Target.FQN)

			changeTarget := bs.Engine.Targets.FindGraph(rr.Target)
			changeHash := bs.Engine.LocalCache.HashInput(changeTarget)

			if currHash != changeHash {
				filteredRRs = append(filteredRRs, rr)
			}
		}
	}

	if len(filteredRRs) == 0 {
		status("Nothing changed!")
		if bs.Engine != s.cbs.Engine {
			bs.Engine.Finalizers.Run(nil)
		}
		return nil
	}

	s.sigCh <- sigEvent{
		bs:     bs,
		rrs:    filteredRRs,
		events: events,
	}
	drainConsole()

	return nil
}

type CancellableCtx struct {
	ctx    context.Context
	cancel context.CancelFunc
	doneCh chan struct{}
}

func (a *CancellableCtx) Cancel() {
	if a.cancel != nil {
		a.cancel()
	}
	if a.doneCh != nil {
		<-a.doneCh
	}
}

func (a *CancellableCtx) CancelAndNew(ctx context.Context) context.Context {
	a.Cancel()
	a.ctx, a.cancel = context.WithCancel(ctx)
	a.doneCh = make(chan struct{})
	return a.ctx
}

func (a *CancellableCtx) Done() {
	close(a.doneCh)
}

func (s *State) watchSigs() error {
	var cc CancellableCtx

	for {
		fmt.Fprintln(log.Writer())
		status("Waiting for change...")
		select {
		case <-s.ctx.Done():
			cc.Cancel()
			return s.ctx.Err()
		case e := <-s.sigCh:
			ctx := cc.CancelAndNew(s.ctx)

			go func() {
				defer cc.Done()

				err := s.handleSig(ctx, e)
				if err != nil {
					if errors.Is(err, context.Canceled) {
						if s.ctx.Err() == nil {
							status("Got changes, killed")
						}
						return
					}

					bootstrap.PrintHumanError(err)
					status("Completed with error")
				}
			}()
		}
	}
}

var (
	ConsoleStdout = NewConsoleWriter(os.Stdout)
	ConsoleStderr = NewConsoleWriter(os.Stderr)
)

func connectConsole() {
	ConsoleStdout.Connect()
	ConsoleStderr.Connect()
}

func disconnectConsole() {
	ConsoleStdout.Disconnect()
	ConsoleStderr.Disconnect()
}

func drainConsole() {
	ConsoleStdout.Drain()
	ConsoleStderr.Drain()
}

func printEvents(events []fsEvent) {
	longest := 0
	for _, event := range events {
		l := len(event.Op.String())
		if l > longest {
			longest = l
		}
	}
	for _, event := range events {
		status(fmt.Sprintf("  %-*v %v", longest, event.Op, event.RelPath))
	}
}

var successStyle = lipgloss.NewStyle().Renderer(log.Renderer()).Foreground(lipgloss.Color("#00FF00"))

func (s *State) handleSig(ctx context.Context, e sigEvent) error {
	status("Got update...")

	if s.cbs.Engine != e.bs.Engine {
		if s.cbs.Finalizers != nil {
			s.cbs.Finalizers.Run(nil)
		}

		s.cbs = e.bs
	}

	for _, path := range s.watcher.WatchList() {
		_ = s.watcher.Remove(path)
	}

	err := s.watcher.Add(e.bs.Root.Root.Abs())
	if err != nil {
		return err
	}

	ancestors, err := e.bs.Graph.DAG().GetOrderedAncestors(e.rrs.Targets().Slice(), true)
	if err != nil {
		return err
	}

	for _, target := range ancestors {
		for _, file := range target.Deps.All().Files {
			dir := filepath.Dir(file.Abs())

			err := s.watcher.Add(dir)
			if err != nil {
				return err
			}
		}
	}

	for _, rr := range e.rrs {
		s.cbs.Engine.LocalCache.ResetCacheHashInput(rr.Target)

		target := e.bs.Engine.Targets.FindGraph(rr.Target)
		hash := e.bs.Engine.LocalCache.HashInput(target)
		s.triggeredHashed.Set(rr.Target.FQN, hash)
	}

	// TODO: Split each rr into its own RunMode so that it can be canceled separately

	connectConsole()
	defer disconnectConsole()

	err = bootstrap.RunMode(ctx, s.cbs.Engine, e.rrs, s.runopts, true, "watch", sandbox.IOConfig{
		Stdout: ConsoleStdout,
		Stderr: ConsoleStderr,
		//Stdin:  os.Stdin,
	})
	if err != nil {
		return err
	}

	status("Completed successfully")

	return nil
}
