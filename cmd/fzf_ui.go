package cmd

import (
	"fmt"
	"github.com/charmbracelet/bubbles/textinput"
	tea "github.com/charmbracelet/bubbletea"
	"heph/cmd/bbt"
	"time"
)

type bbtfzf struct {
	ti          textinput.Model
	suggestions []string
	targets     []string
	debounce    bbt.Debounce
}

type bbtfzfSuggestions []string

func newBbtFzf(targets []string) bbtfzf {
	ti := textinput.New()
	ti.Placeholder = "Search..."
	ti.Focus()

	return bbtfzf{
		targets:  targets,
		ti:       ti,
		debounce: bbt.NewDebounce(50 * time.Millisecond),
	}
}

func (m bbtfzf) Init() tea.Cmd {
	return textinput.Blink
}

func (m bbtfzf) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	var cmd tea.Cmd

	switch msg := msg.(type) {
	case tea.KeyMsg:
		switch msg.Type {
		case tea.KeyCtrlC:
			return m, tea.Quit
		case tea.KeyEnter:
			return m, func() tea.Msg {
				return tea.EnterAltScreen()
			}
		case tea.KeyEsc:
			return m, func() tea.Msg {
				return tea.ExitAltScreen()
			}
		}
	case bbtfzfSuggestions:
		m.suggestions = msg
	}

	m.ti, cmd = m.ti.Update(msg)

	return m, tea.Batch(
		cmd,
		m.debounce.Do(func() tea.Msg {
			suggestions := fuzzyFindTargetName(m.targets, m.ti.Value(), 10)
			return bbtfzfSuggestions(suggestions)
		}),
	)
}

func (m bbtfzf) View() string {
	s := m.ti.View() + "\n"

	for _, sugg := range m.suggestions {
		s += fmt.Sprintf("%s\n", sugg)
	}

	return s
}
