package cmd

import (
	"bytes"
	"context"
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

func DynamicRenderer(ctx context.Context, cancel func(), pool *worker.Pool) error {
	doneCh := pool.Done()

	p := tea.NewProgram(renderer{
		start:  time.Now(),
		cancel: cancel,
		UpdateMessage: UpdateMessage{
			jobs:    pool.JobCount,
			done:    pool.DoneCount,
			workers: pool.Workers,
		},
	})

	go func() {
		for {
			time.Sleep(50 * time.Millisecond)

			p.Send(UpdateMessage{
				jobs:    pool.JobCount,
				done:    pool.DoneCount,
				workers: pool.Workers,
			})

			select {
			case <-doneCh:
				p.Send(UpdateMessage{
					jobs:    pool.JobCount,
					done:    pool.DoneCount,
					workers: pool.Workers,
				})
				p.Quit()
				return
			default:
			}
		}
	}()

	running := false
	log.AddHook(hookFunc{
		f: func(entry *log.Entry) {
			if !running {
				return
			}

			if !log.StandardLogger().IsLevelEnabled(entry.Level) {
				return
			}

			b, err := log.StandardLogger().Formatter.Format(entry)
			if err != nil {
				fmt.Println(fmt.Errorf("logger fmt error: %v", err))
				return
			}

			p.Printf("%s", bytes.TrimSpace(b))
		},
	})

	prevOut := log.StandardLogger().Out
	log.SetOutput(io.Discard)
	defer func() {
		running = false
		log.SetOutput(prevOut)
	}()

	go func() {
		<-ctx.Done()
		p.Quit()
	}()

	running = true
	err := p.Start()
	if err != nil {
		return err
	}

	if ctx.Err() != nil {
		os.Exit(1)
	}

	return nil
}

type UpdateMessage struct {
	workers []*worker.Worker
	jobs    uint64
	done    uint64
}

type renderer struct {
	start  time.Time
	cancel func()
	UpdateMessage
}

func (r renderer) Init() tea.Cmd {
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
	if r.done == r.jobs {
		return fmt.Sprintf("Ran %v jobs in %v\n", r.done, round(time.Now().Sub(r.start), 1).String())
	}

	var s strings.Builder
	s.WriteString(fmt.Sprintf("%v/%v %v\n", r.done, r.jobs, round(time.Now().Sub(r.start), 1).String()))

	for _, w := range r.workers {
		var runtime string
		state := "I"
		if w.CurrentJob != nil {
			state = "R"
			runtime = " " + round(time.Now().Sub(w.CurrentJob.TimeStart), 1).String()
		}

		status := w.GetStatus()
		if status == "" {
			status = "Waiting..."
		}

		s.WriteString(fmt.Sprintf("  %v%v %v\n", state, runtime, status))
	}

	return s.String()
}
