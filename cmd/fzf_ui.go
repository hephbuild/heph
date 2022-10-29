package cmd

import (
	"fmt"
	"github.com/charmbracelet/bubbles/textinput"
	tea "github.com/charmbracelet/bubbletea"
)

type bbtfzf struct {
	ti          textinput.Model
	suggestions []string
	targets     []string
}

func newBbtFzf(targets []string) bbtfzf {
	ti := textinput.New()
	ti.Placeholder = "Search..."
	ti.Focus()

	return bbtfzf{
		targets: targets,
		ti:      ti,
	}
}

func (m bbtfzf) Init() tea.Cmd {
	return textinput.Blink
}

func (m bbtfzf) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	var cmd tea.Cmd

	switch msg := msg.(type) {

	case tea.KeyMsg:
		switch msg.String() {

		case "ctrl+c":
			return m, tea.Quit
		}
	}

	m.ti, cmd = m.ti.Update(msg)
	m.suggestions = fuzzyFindTargetName(m.targets, m.ti.Value(), 10)

	return m, cmd
}

func (m bbtfzf) View() string {
	s := m.ti.View() + "\n"

	for _, sugg := range m.suggestions {
		s += fmt.Sprintf("%s\n", sugg)
	}

	return s
}
