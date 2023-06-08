package poolui

import (
	"errors"
	"fmt"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils"
	"github.com/hephbuild/heph/worker"
	"github.com/mattn/go-isatty"
	"go.uber.org/multierr"
	"os"
	"runtime/debug"
	"strings"
	"sync"
	"time"
)

var isTerm bool

func init() {
	if os.Stderr != nil {
		isTerm = isatty.IsTerminal(os.Stderr.Fd())
	}
}

func logUI(name string, deps *worker.WaitGroup, pool *worker.Pool) error {
	start := time.Now()
	printProgress := func() {
		s := deps.TransitiveCount()
		log.Infof("Progress %v: %v/%v %v", name, s.Done, s.All, utils.RoundDuration(time.Since(start), 1).String())
	}

	printWorkersStatus := func() {
		for _, w := range pool.Workers {
			j := w.CurrentJob
			if j == nil {
				continue
			}

			duration := time.Since(j.TimeStart)

			if duration < 5*time.Second {
				// Skip printing short jobs
				continue
			}

			status := w.GetStatus().String(log.Renderer())
			if status == "" {
				status = "Waiting..."
			}

			runtime := fmt.Sprintf("%v", utils.RoundDuration(duration, 1).String())

			fmt.Fprintf(os.Stderr, " %v %v\n", runtime, status)
		}
	}

	t := time.NewTicker(time.Second)
	defer t.Stop()

	c := 1
	for {
		select {
		case <-t.C:
			printProgress()
			if c >= 5 {
				c = 1
				printWorkersStatus()
			}
			c++
			continue
		case <-deps.Done():
			// will break
		}

		printProgress()
		return nil
	}
}

func Wait(name string, pool *worker.Pool, deps *worker.WaitGroup, plain bool) error {
	tui := isTerm && !plain

	log.Tracef("WaitPool %v", name)
	defer func() {
		log.Tracef("WaitPool %v DONE", name)
	}()

	if tui {
		err := interactiveUI(name, deps, pool)
		if err != nil {
			return fmt.Errorf("poolui: %w", err)
		}
	} else {
		err := logUI(name, deps, pool)
		if err != nil {
			return fmt.Errorf("logpoolui: %w", err)
		}
	}

	perr := pool.Err()
	derr := deps.Err()

	if perr != nil && derr != nil {
		if errors.Is(perr, derr) || errors.Is(derr, perr) {
			return perr
		}

		perr = fmt.Errorf("pool: %w", perr)
		derr = fmt.Errorf("deps: %w", derr)
	}

	return multierr.Combine(perr, derr)
}

var TUIm sync.Mutex
var TUIStack []byte

func interactiveUI(name string, deps *worker.WaitGroup, pool *worker.Pool) error {
	if !TUIm.TryLock() {
		//panic(fmt.Sprintf("concurrent call of poolui.Wait, already running at:\n%s\ntrying to run at", stack))
		return logUI(name, deps, pool)
	}

	TUIStack = debug.Stack()

	defer func() {
		TUIStack = nil
		TUIm.Unlock()
	}()

	msg := func() UpdateMessage {
		s := deps.TransitiveCount()
		return UpdateMessage{
			stats:   s,
			workers: pool.Workers,
		}
	}

	r := &view{
		name:  name,
		pool:  pool,
		start: time.Now(),
		cancel: func() {
			pool.Stop(fmt.Errorf("user canceled"))
		},
		UpdateMessage: msg(),
	}

	p := tea.NewProgram(r, tea.WithOutput(os.Stderr), tea.WithoutSignalHandler())

	r.onTermWidth = func(w int) {
		log.SetPrint(w, func(s string) {
			p.Println(s)
		})
	}
	r.onInit = func() {
		r.onTermWidth(0)

		go func() {
			t := time.NewTicker(50 * time.Millisecond)
			defer t.Stop()

			for {
				select {
				case <-deps.Done():
					m := msg()
					m.summary = true
					p.Send(m)
					p.Quit()
					return
				case <-t.C:
					p.Send(msg())
				}
			}
		}()
	}

	_, err := p.Run()
	log.SetPrint(0, nil)
	_ = p.ReleaseTerminal()
	if err != nil {
		return err
	}

	if !deps.IsDone() {
		pool.Stop(fmt.Errorf("TUI exited unexpectedly"))
	}

	return nil
}

type UpdateMessage struct {
	workers []*worker.Worker
	stats   worker.WaitGroupStats
	summary bool
}

type view struct {
	name        string
	start       time.Time
	cancel      func()
	pool        *worker.Pool
	onTermWidth func(w int)
	onInit      func()
	UpdateMessage
}

func (r *view) Init() tea.Cmd {
	r.onInit()
	return nil
}

func printJobsWaitStack(jobs []*worker.Job, d int) []string {
	prefix := strings.Repeat("  ", d+1)

	strs := make([]string, 0)
	for _, j := range jobs {
		if j.IsDone() {
			continue
		}

		strs = append(strs, fmt.Sprintf("%v- %v (%v)", prefix, j.Name, j.State.String()))

		deps := j.Deps.Jobs()
		if len(deps) > 0 {
			strs = append(strs, prefix+fmt.Sprintf("  deps: (%v)", len(deps)))
			strs = append(strs, printJobsWaitStack(deps, d+1)...)
		}
	}

	return strs
}

func (r *view) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case UpdateMessage:
		r.UpdateMessage = msg
	case tea.KeyMsg:
		switch msg.Type {
		case tea.KeyBreak:
			r.cancel()
			return r, nil
		case tea.KeyRunes:
			switch msg.String() {
			case "p":
				strs := make([]string, 0)
				strs = append(strs, "Unfinished jobs:")

				strs = append(strs, printJobsWaitStack(r.pool.Jobs(), 0)...)

				return r, tea.Batch(tea.Println(strings.Join(strs, "\n")))
			}
		}
	case tea.WindowSizeMsg:
		r.onTermWidth(msg.Width)
	}

	return r, nil
}

var lrenderer = log.Renderer()

var styleWorkerStart = lipgloss.NewStyle().Renderer(lrenderer).Bold(true)
var styleFaint = lipgloss.NewStyle().Renderer(lrenderer).Faint(true)

func (r *view) View() string {
	start := utils.RoundDuration(time.Since(r.start), 1).String()

	if r.summary {
		count := fmt.Sprint(r.stats.Done)
		if r.stats.Done != r.stats.All {
			count = fmt.Sprintf("%v/%v", r.stats.Done, r.stats.All)
		}
		extra := ""
		if r.stats.Failed > 0 || r.stats.Skipped > 0 {
			extra = fmt.Sprintf(" (%v failed, %v skipped)", r.stats.Failed, r.stats.Skipped)
		}
		return fmt.Sprintf("%v: Ran %v jobs in %v%v\n", r.name, count, start, extra)
	}

	var s strings.Builder
	s.WriteString(fmt.Sprintf("%v: %v/%v %v\n", r.name, r.stats.Done, r.stats.All, start))
	if r.stats.Failed > 0 || r.stats.Skipped > 0 {
		s.WriteString(fmt.Sprintf("%v failed, %v skipped\n", r.stats.Failed, r.stats.Skipped))
	}

	for _, w := range r.workers {
		runtime := ""
		if j := w.CurrentJob; j != nil {
			runtime = fmt.Sprintf("=> [%5s]", utils.FormatDuration(time.Since(j.TimeStart)))
		}

		status := w.GetStatus().String(log.Renderer())
		if status == "" {
			status = styleFaint.Render("=|")
		}

		s.WriteString(fmt.Sprintf("%v %v\n", styleWorkerStart.Render(runtime), status))
	}

	return s.String()
}
