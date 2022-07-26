package cmd

import (
	"bytes"
	"fmt"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/muesli/reflow/wrap"
	log "github.com/sirupsen/logrus"
	"go.uber.org/multierr"
	"heph/utils"
	"heph/worker"
	"io"
	"os"
	"strings"
	"time"
)

type hookFunc struct {
	f func(*log.Entry)
}

func (h hookFunc) Levels() []log.Level {
	return log.AllLevels
}

func (h hookFunc) Fire(entry *log.Entry) error {
	h.f(entry)

	return nil
}

func logPoolUI(name string, deps *worker.WaitGroup, pool *worker.Pool) error {
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

			status := w.GetStatus()
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

func WaitPool(name string, pool *worker.Pool, deps *worker.WaitGroup, forceSilent bool) error {
	log.Tracef("WaitPool %v", name)
	defer func() {
		log.Tracef("WaitPool %v DONE", name)
	}()

	if !forceSilent && isTerm && !*plain {
		err := poolUI(name, deps, pool)
		if err != nil {
			return fmt.Errorf("poolui: %w", err)
		}
	} else {
		err := logPoolUI(name, deps, pool)
		if err != nil {
			return fmt.Errorf("logpoolui: %w", err)
		}
	}

	perr := pool.Err()
	derr := deps.Err()

	if perr != nil && derr != nil {
		if perr.Error() == derr.Error() {
			return perr
		}

		perr = fmt.Errorf("pool: %w", perr)
		derr = fmt.Errorf("deps: %w", derr)
	}

	return multierr.Combine(perr, derr)
}

func poolUI(name string, deps *worker.WaitGroup, pool *worker.Pool) error {
	msg := func() UpdateMessage {
		s := deps.TransitiveCount()

		return UpdateMessage{
			stats:   s,
			workers: pool.Workers,
		}
	}

	r := &renderer{
		name:  name,
		pool:  pool,
		start: time.Now(),
		cancel: func() {
			pool.Stop(fmt.Errorf("user canceled"))
		},
		UpdateMessage: msg(),
	}

	p := tea.NewProgram(r, tea.WithOutput(os.Stderr))

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

	log.AddHook(hookFunc{
		f: func(entry *log.Entry) {
			if !r.running {
				return
			}

			if !entry.Logger.IsLevelEnabled(entry.Level) {
				return
			}

			b, err := entry.Logger.Formatter.Format(entry)
			if err != nil {
				p.Printf(fmt.Sprintf("logger fmt error: %v", err))
				return
			}

			b = bytes.TrimSpace(b)
			if r.termWidth > 0 && len(b) > r.termWidth {
				b = wrap.Bytes(b, r.termWidth)
			}
			p.Printf("%s", b)
		},
	})

	prevOut := log.StandardLogger().Out
	log.SetOutput(io.Discard)

	err := p.Start()
	r.running = false
	log.SetOutput(prevOut)
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

type renderer struct {
	name      string
	running   bool
	start     time.Time
	cancel    func()
	sb        strings.Builder
	pool      *worker.Pool
	termWidth int
	UpdateMessage
}

func (r *renderer) Init() tea.Cmd {
	r.running = true
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

func (r *renderer) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
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
		r.termWidth = msg.Width
	}

	return r, nil
}

func (r *renderer) View() string {
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
		state := "I"
		runtime := ""
		if j := w.CurrentJob; j != nil {
			state = "R"
			runtime = fmt.Sprintf(" %v", utils.RoundDuration(time.Since(j.TimeStart), 1).String())
		}

		status := w.GetStatus()
		if status == "" {
			status = "Waiting..."
		}

		s.WriteString(fmt.Sprintf("  %v%v %v\n", state, runtime, status))
	}

	return s.String()
}
