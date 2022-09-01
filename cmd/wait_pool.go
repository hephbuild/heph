package cmd

import (
	"bytes"
	"fmt"
	tea "github.com/charmbracelet/bubbletea"
	log "github.com/sirupsen/logrus"
	"heph/worker"
	"io"
	"os"
	"strings"
	"time"
)

var divs = []time.Duration{
	time.Duration(1), time.Duration(10), time.Duration(100), time.Duration(1000)}

func round(d time.Duration, digits int) time.Duration {
	switch {
	case d > time.Second:
		d = d.Round(time.Second / divs[digits])
	case d > time.Millisecond:
		d = d.Round(time.Millisecond / divs[digits])
	case d > time.Microsecond:
		d = d.Round(time.Microsecond / divs[digits])
	}
	return d
}

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

	if !forceSilent && isTerm && !*plain {
		err := PoolUI(name, deps, Engine.Pool)
		if err != nil {
			return fmt.Errorf("dynamic renderer: %w", err)
		}
	} else {
		<-deps.Done()
	}

	if err := Engine.Pool.Err(); err != nil {
		return fmt.Errorf("pool: %w", err)
	}

	return deps.Err()
}

func PoolUI(name string, deps *worker.WaitGroup, pool *worker.Pool) error {
	log.Tracef("POOL UI %v", name)

	msg := func() UpdateMessage {
		return UpdateMessage{
			jobs:    deps.TransitiveJobCount(),
			success: deps.TransitiveSuccessCount(),
			workers: pool.Workers,
		}
	}

	r := &renderer{
		name:  name,
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

	// Make sure all is done
	<-deps.Done()

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
	UpdateMessage
}

func (r renderer) Init() tea.Cmd {
	r.running = true
	return nil
}

func (r renderer) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case UpdateMessage:
		r.UpdateMessage = msg
	case tea.KeyMsg:
		switch msg.Type {
		case tea.KeyBreak:
			r.cancel()
			return r, tea.Quit
		}
	}

	return r, nil
}

func (r renderer) View() string {
	if r.summary {
		count := fmt.Sprint(r.success)
		if r.success != r.jobs {
			count = fmt.Sprintf("%v/%v", r.success, r.jobs)
		}
		return fmt.Sprintf("%v: Ran %v jobs in %v\n", r.name, count, round(time.Now().Sub(r.start), 1).String())
	}

	var s strings.Builder
	s.WriteString(fmt.Sprintf("%v: %v/%v %v\n", r.name, r.success, r.jobs, round(time.Now().Sub(r.start), 1).String()))

	for _, w := range r.workers {
		var runtime string
		state := "I"
		if j := w.CurrentJob; j != nil {
			state = "R"
			runtime = fmt.Sprintf(" %v", round(time.Now().Sub(j.TimeStart), 1).String())
		}

		status := w.GetStatus()
		if status == "" {
			status = "Waiting..."
		}

		s.WriteString(fmt.Sprintf("  %v%v %v\n", state, runtime, status))
	}

	return s.String()
}
