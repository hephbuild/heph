package cmd

import (
	"bytes"
	"fmt"
	tea "github.com/charmbracelet/bubbletea"
	log "github.com/sirupsen/logrus"
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

func WaitPool(name string, deps *worker.WaitGroup, forceSilent bool) error {
	log.Tracef("WaitPool %v", name)
	defer func() {
		log.Tracef("WaitPool %v DONE", name)
	}()

	pool := Engine.Pool

	if !forceSilent && isTerm && !*plain {
		err := PoolUI(name, deps, pool)
		if err != nil {
			return fmt.Errorf("poolui: %w", err)
		}
	} else {
		printProgress := func() {
			log.Infof("Progress %v: %v/%v", name, deps.TransitiveSuccessCount(), deps.TransitiveJobCount())
		}
		for {
			select {
			case <-time.After(time.Second):
				printProgress()
				continue
			case <-deps.Done():
				// will break
			}

			printProgress()
			break
		}
	}

	if err := pool.Err(); err != nil {
		return fmt.Errorf("pool: %w", err)
	}

	return deps.Err()
}

func PoolUI(name string, deps *worker.WaitGroup, pool *worker.Pool) error {
	msg := func() UpdateMessage {
		return UpdateMessage{
			jobs:    deps.TransitiveJobCount(),
			success: deps.TransitiveSuccessCount(),
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
		for {
			select {
			case <-deps.Done():
				m := msg()
				m.summary = true
				p.Send(m)
				p.Quit()
				return
			case <-time.After(50 * time.Millisecond):
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

			p.Printf("%s", bytes.TrimSpace(b))
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
	jobs    uint64
	success uint64
	summary bool
}

type renderer struct {
	name    string
	running bool
	start   time.Time
	cancel  func()
	sb      strings.Builder
	pool    *worker.Pool
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

		strs = append(strs, fmt.Sprintf("%v- %v (%v)", prefix, j.ID, j.State.String()))

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
	}

	return r, nil
}

func (r *renderer) View() string {
	start := utils.RoundTime(time.Since(r.start), 1).String()

	if r.summary {
		count := fmt.Sprint(r.success)
		if r.success != r.jobs {
			count = fmt.Sprintf("%v/%v", r.success, r.jobs)
		}
		return fmt.Sprintf("%v: Ran %v jobs in %v\n", r.name, count, start)
	}

	var s strings.Builder
	s.WriteString(fmt.Sprintf("%v: %v/%v %v\n", r.name, r.success, r.jobs, start))

	for _, w := range r.workers {
		var runtime string
		state := "I"
		if j := w.CurrentJob; j != nil {
			state = "R"
			runtime = fmt.Sprintf(" %v", utils.RoundTime(time.Since(j.TimeStart), 1).String())
		}

		status := w.GetStatus()
		if status == "" {
			status = "Waiting..."
		}

		s.WriteString(fmt.Sprintf("  %v%v %v\n", state, runtime, status))
	}

	return s.String()
}
