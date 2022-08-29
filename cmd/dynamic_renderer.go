package cmd

import (
	"context"
	"fmt"
	tea "github.com/charmbracelet/bubbletea"
	"heph/log"
	"heph/worker"
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

func DynamicRenderer(name string, ctx context.Context, cancel func(), pool *worker.Pool) error {
	doneCh := pool.Done()

	p := tea.NewProgram(renderer{
		name:   name,
		start:  time.Now(),
		cancel: cancel,
		UpdateMessage: UpdateMessage{
			jobs:    pool.JobCount,
			done:    pool.DoneCount,
			workers: pool.Workers,
		},
	}, tea.WithOutput(os.Stderr), tea.WithoutCatchPanics())

	go func() {
		for {
			select {
			case <-doneCh:
				p.Send(UpdateMessage{
					jobs:    pool.JobCount,
					done:    pool.DoneCount,
					workers: pool.Workers,
					summary: true,
				})
				p.Quit()
				return
			case <-time.After(50 * time.Millisecond):
				p.Send(UpdateMessage{
					jobs:    pool.JobCount,
					done:    pool.DoneCount,
					workers: pool.Workers,
				})
			}
		}
	}()

	go func() {
		<-ctx.Done()
		p.Quit()
	}()

	log.SetPrintfFunc(p.Printf)
	err := p.Start()
	log.SetPrintfFunc(nil)
	if err != nil {
		return err
	}

	if err := ctx.Err(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}

	return nil
}

type UpdateMessage struct {
	workers []*worker.Worker
	jobs    uint64
	done    uint64
	summary bool
}

type renderer struct {
	name   string
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
	if r.summary {
		count := fmt.Sprint(r.done)
		if r.done != r.jobs {
			count = fmt.Sprintf("%v/%v", r.done, r.jobs)
		}
		return fmt.Sprintf("%v: Ran %v jobs in %v\n", r.name, count, round(time.Now().Sub(r.start), 1).String())
	}

	var s strings.Builder
	s.WriteString(fmt.Sprintf("%v: %v/%v %v\n", r.name, r.done, r.jobs, round(time.Now().Sub(r.start), 1).String()))

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
