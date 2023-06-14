package poolui

import (
	"context"
	"fmt"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils/xcontext"
	"github.com/hephbuild/heph/utils/xtime"
	"github.com/hephbuild/heph/worker"
	"strings"
	"time"
)

type UpdateMessage struct {
	workers []*worker.Worker
	stats   worker.WaitGroupStats
	final   bool
}

func New(ctx context.Context, name string, deps *worker.WaitGroup, pool *worker.Pool, quitWhenDone bool) *Model {
	return &Model{
		name:  name,
		deps:  deps,
		pool:  pool,
		start: time.Now(),
		cancel: func() {
			xcontext.Cancel(ctx)
		},
		logEntryCh:   make(chan log.FormattableEntry),
		quitWhenDone: quitWhenDone,
	}
}

type Model struct {
	name         string
	deps         *worker.WaitGroup
	start        time.Time
	cancel       func()
	pool         *worker.Pool
	logEntryCh   chan log.FormattableEntry
	width        int
	quitWhenDone bool
	UpdateMessage
}

func (m *Model) Init() tea.Cmd {
	log.SetDiversion(m.logEntryCh)
	m.UpdateMessage = m.updateMsg(false)
	return tea.Batch(
		m.nextLogEntryCmd,
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
		final = m.deps.IsDone()
	}

	s := m.deps.TransitiveCount()
	return UpdateMessage{
		stats:   s,
		workers: m.pool.Workers,
		final:   final,
	}
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

func (m *Model) nextLogEntryCmd() tea.Msg {
	return <-m.logEntryCh
}

func (m *Model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.KeyMsg:
		switch msg.Type {
		case tea.KeyBreak:
			m.cancel()
			return m, nil
		case tea.KeyRunes:
			switch msg.String() {
			case "p":
				strs := make([]string, 0)
				strs = append(strs, "Unfinished jobs:")

				strs = append(strs, printJobsWaitStack(m.pool.Jobs(), 0)...)

				return m, tea.Println(strings.Join(strs, "\n"))
			}
		}
	case tea.WindowSizeMsg:
		m.width = msg.Width

	case UpdateMessage:
		m.UpdateMessage = msg
		if msg.final {
			if m.quitWhenDone {
				return m, func() tea.Msg {
					return tea.Quit()
				}
			}
		}
		return m, m.doUpdateMsgTicker()
	case log.FormattableEntry:
		return m, tea.Batch(func() tea.Msg {
			return tea.Println(msg.Format(m.width))()
		}, m.nextLogEntryCmd)
	}

	return m, nil
}

var lrenderer = log.Renderer()

var styleWorkerStart = lipgloss.NewStyle().Renderer(lrenderer).Bold(true)
var styleFaint = lipgloss.NewStyle().Renderer(lrenderer).Faint(true)

func (m *Model) View() string {
	start := xtime.RoundDuration(time.Since(m.start), 1).String()

	if m.final {
		count := fmt.Sprint(m.stats.Done)
		if m.stats.Done != m.stats.All {
			count = fmt.Sprintf("%v/%v", m.stats.Done, m.stats.All)
		}
		extra := ""
		if m.stats.Failed > 0 || m.stats.Skipped > 0 {
			extra = fmt.Sprintf(" (%v failed, %v skipped)", m.stats.Failed, m.stats.Skipped)
		}
		return fmt.Sprintf("%v: Ran %v jobs in %v%v\n", m.name, count, start, extra)
	}

	var s strings.Builder
	s.WriteString(fmt.Sprintf("%v: %v/%v %v\n", m.name, m.stats.Done, m.stats.All, start))
	if m.stats.Failed > 0 || m.stats.Skipped > 0 {
		s.WriteString(fmt.Sprintf("%v failed, %v skipped\n", m.stats.Failed, m.stats.Skipped))
	}

	for _, w := range m.workers {
		runtime := ""
		if j := w.CurrentJob; j != nil {
			runtime = fmt.Sprintf("=> [%5s]", xtime.FormatDuration(time.Since(j.TimeStart)))
		}

		status := w.GetStatus().String(log.Renderer())
		if status == "" {
			status = styleFaint.Render("=|")
		}

		s.WriteString(fmt.Sprintf("%v %v\n", styleWorkerStart.Render(runtime), status))
	}

	return s.String()
}
