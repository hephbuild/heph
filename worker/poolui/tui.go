package poolui

import (
	"context"
	"fmt"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils/xcontext"
	"github.com/hephbuild/heph/utils/xtea"
	"github.com/hephbuild/heph/utils/xtime"
	"github.com/hephbuild/heph/worker2"
	"strings"
	"time"
)

type UpdateMessage struct {
	workers []*worker2.Worker
	stats   worker2.Stats
	final   bool
}

func New(ctx context.Context, name string, deps worker2.Dep, pool *worker2.Engine, quitWhenDone bool) *Model {
	return &Model{
		name:  name,
		deps:  deps,
		pool:  pool,
		start: time.Now(),
		cancel: func() {
			xcontext.Cancel(ctx)
		},
		log:          xtea.NewLogModel(),
		quitWhenDone: quitWhenDone,
	}
}

type Model struct {
	name         string
	deps         worker2.Dep
	start        time.Time
	cancel       func()
	pool         *worker2.Engine
	log          xtea.LogModel
	quitWhenDone bool
	UpdateMessage
}

func (m *Model) Init() tea.Cmd {
	m.log.Init()
	m.UpdateMessage = m.updateMsg(false)
	return tea.Batch(
		m.log.Next,
		m.doUpdateMsgTicker(),
	)
}

func (m *Model) doUpdateMsgTicker() tea.Cmd {
	return tea.Tick(50*time.Millisecond, func(time.Time) tea.Msg {
		return m.updateMsg(false)
	})
}

func (m *Model) updateMsg(final bool) UpdateMessage {
	if !final {
		select {
		case <-m.deps.Wait():
			final = true
		default:
			final = false
		}
	}

	s := worker2.CollectStats(m.deps)
	return UpdateMessage{
		stats:   s,
		workers: m.pool.GetWorkers(),
		final:   final,
	}
}

func (m *Model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	var (
		cmd  tea.Cmd
		cmds []tea.Cmd
	)

	switch msg := msg.(type) {
	case tea.KeyMsg:
		switch msg.Type {
		case tea.KeyBreak:
			m.cancel()
			return m, nil
		}
	case UpdateMessage:
		m.UpdateMessage = msg
		if msg.final {
			if m.quitWhenDone {
				return m, tea.Quit
			}
		}
		return m, m.doUpdateMsgTicker()
	}

	m.log, cmd = m.log.Update(msg)
	cmds = append(cmds, cmd)

	return m, tea.Batch(cmds...)
}

var styleWorkerStart = lipgloss.NewStyle().Renderer(log.Renderer()).Bold(true)
var styleFaint = lipgloss.NewStyle().Renderer(log.Renderer()).Faint(true)

func (m *Model) View() string {
	start := xtime.RoundDuration(time.Since(m.start), 1).String()

	if m.final {
		count := fmt.Sprint(m.stats.Completed)
		if m.stats.Completed != m.stats.All {
			count = fmt.Sprintf("%v/%v", m.stats.Completed, m.stats.All)
		}
		extra := ""
		if m.stats.Failed > 0 || m.stats.Skipped > 0 {
			extra = fmt.Sprintf(" (%v failed, %v skipped)", m.stats.Failed, m.stats.Skipped)
		}
		return fmt.Sprintf("%v: Ran %v jobs in %v%v\n", m.name, count, start, extra)
	}

	var s strings.Builder
	s.WriteString(fmt.Sprintf("%v: %v/%v %v", m.name, m.stats.Completed, m.stats.All, start))
	if m.stats.Suspended > 0 {
		s.WriteString(fmt.Sprintf(" (%v suspended)", m.stats.Suspended))
	}
	s.WriteString("\n")
	if m.stats.Failed > 0 || m.stats.Skipped > 0 {
		s.WriteString(fmt.Sprintf("%v failed, %v skipped\n", m.stats.Failed, m.stats.Skipped))
	}

	for _, w := range m.workers {
		runtime := ""
		if j := w.Execution(); j != nil {
			runtime = fmt.Sprintf("=> [%5s]", xtime.FormatFixedWidthDuration(time.Since(j.StartedAt)))
		}

		status := w.GetStatus().String(log.Renderer())
		if status == "" {
			status = styleFaint.Render("=|")
		}

		s.WriteString(fmt.Sprintf("%v %v\n", styleWorkerStart.Render(runtime), status))
	}

	return s.String()
}

func (m *Model) Clean() {
	m.log.Clean()
}
