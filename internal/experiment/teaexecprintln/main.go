package main

import (
	"context"
	"fmt"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/hephbuild/hephv2/internal/hbbt/hbbtexec"
	"github.com/hephbuild/hephv2/internal/hbbt/hbbtlog"
	"github.com/hephbuild/hephv2/internal/hcore/hlog"
	"io"
	"log/slog"
	"os"
	"os/signal"
	"time"
)

// When exec is running, its blocking the event loop
// A side effect is that tea.Println dont go through

type execCmd struct {
	w   io.Writer
	log hbbtlog.Hijacker
	ctx context.Context
}

func (e *execCmd) Run() error {
	c := make(chan os.Signal, 1)
	signal.Notify(c, os.Interrupt)
	defer signal.Stop(c)
	defer e.w.Write([]byte("\n"))

	e.w.Write([]byte(fmt.Sprintf("exec write\n")))

	hlog.From(e.ctx).Info(fmt.Sprintf("exec log"))

	select {
	case s := <-c:
		e.w.Write([]byte("signal: " + s.String() + "\n"))
		return nil
	case <-time.After(time.Second):

	}

	return nil
}

func (e *execCmd) SetStdin(r io.Reader) {
}

func (e *execCmd) SetStdout(w io.Writer) {
	e.w = w
}

func (e *execCmd) SetStderr(w io.Writer) {
}

type TickMsg time.Time

func tickEvery() tea.Cmd {
	return tea.Every(time.Second, func(t time.Time) tea.Msg {
		return TickMsg(t)
	})
}

type model struct {
	ctx  context.Context
	i    int
	log  hbbtlog.Hijacker
	exec hbbtexec.Model
}

func (m model) Init() tea.Cmd {
	return tea.Batch(
		m.log.Init(),
		m.exec.Exec(&execCmd{ctx: m.ctx, log: m.log}, func(err error) tea.Msg {
			return tea.Quit()
		}),
		tickEvery(),
	)
}

func (m model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	var cmd tea.Cmd
	var cmds []tea.Cmd

	switch msg := msg.(type) {
	case TickMsg:
		_ = msg
		m.i++
		hlog.From(m.ctx).Info(fmt.Sprintf("update %v", m.i)) // these will not appear since exec is blocking
		return m, tea.Batch(tickEvery())
	case tea.KeyMsg:
		switch msg.String() {
		case "ctrl+c", "q":
			return m, tea.Quit
		}
	}

	// log updates need to be handled in update
	m.log, cmd = m.log.Update(msg)
	cmds = append(cmds, cmd)

	return m, tea.Batch(cmds...)
}

func (m model) View() string {
	return fmt.Sprintf("Running command %v...\n", m.i)
}

func main() {
	ctx := context.Background()

	logger := hlog.NewTextLogger(os.Stderr, slog.LevelDebug)
	ctx = hlog.ContextWithLogger(ctx, logger)

	initialModel := model{
		ctx: ctx,
		log: hbbtlog.NewLogHijacker(),
	}
	initialModel.ctx, initialModel.exec = hbbtexec.New(initialModel.ctx, initialModel.log)

	if _, err := tea.NewProgram(initialModel).Run(); err != nil {
		fmt.Println("Error:", err)
		os.Exit(1)
	}
}
