package tuistatus

import (
	tea "github.com/charmbracelet/bubbletea"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils/xtea"
	"time"
)

type model struct {
	f       func()
	log     xtea.LogModel
	h       *handler
	quiting bool
}

func (m model) Init() tea.Cmd {
	return tea.Batch(
		m.log.Next,
		m.doTicker(),
		func() tea.Msg {
			m.f()
			return tea.Quit()
		},
	)
}

type tickMsg time.Time

func (m model) doTicker() tea.Cmd {
	return tea.Tick(50*time.Millisecond, func(time.Time) tea.Msg {
		return tickMsg(time.Now())
	})
}

func (m model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	var (
		cmd  tea.Cmd
		cmds []tea.Cmd
	)

	switch msg.(type) {
	case tickMsg:
		return m, m.doTicker()
	case tea.QuitMsg:
		m.quiting = true
	}

	m.log, cmd = m.log.Update(msg)
	cmds = append(cmds, cmd)

	return m, tea.Batch(cmds...)
}

func (m model) View() string {
	if m.quiting {
		return ""
	}

	if m.h.status == nil {
		return ""
	}

	return m.h.status.String(log.Renderer())
}

func (m model) Clean() {
	m.log.Clean()
}
