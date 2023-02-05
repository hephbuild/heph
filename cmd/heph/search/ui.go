package search

import (
	"github.com/charmbracelet/bubbles/textinput"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"heph/cmd/heph/bbt"
	"heph/targetspec"
	"time"
)

func TUI(targets targetspec.TargetSpecs) error {
	p := tea.NewProgram(newBbtSearch(targets))
	if err := p.Start(); err != nil {
		return err
	}
	return nil
}

type bbtfzf struct {
	ti           textinput.Model
	searchResult bbtSearchResult
	targets      targetspec.TargetSpecs
	debounce     bbt.Debounce
	cursor       int
	search       Func
}

type bbtSearchResult struct {
	query string
	res   Result
	err   error
}

func newBbtSearch(targets targetspec.TargetSpecs) bbtfzf {
	ti := textinput.New()
	ti.Placeholder = "Search..."
	ti.Focus()

	search, err := NewSearch(targets)
	if err != nil {
		panic(err)
	}

	return bbtfzf{
		targets:  targets,
		ti:       ti,
		cursor:   -1,
		search:   search,
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
		case tea.KeyUp:
			if m.cursor >= 0 {
				m.cursor--
			}
		case tea.KeyDown:
			if m.cursor < len(m.searchResult.res.Targets) {
				m.cursor++
			}
		}
	case bbtSearchResult:
		m.searchResult = msg
		if m.cursor > len(m.searchResult.res.Targets)-1 {
			m.cursor = len(m.searchResult.res.Targets) - 1
		}
	}

	var searchm tea.Cmd
	if query := m.ti.Value(); query != m.searchResult.query {
		searchm = m.debounce.Do(func() tea.Msg {
			res, err := m.search(query, 10)
			return bbtSearchResult{res: res, err: err, query: query}
		})
	}

	m.ti, cmd = m.ti.Update(msg)

	return m, tea.Batch(cmd, searchm)
}

var styleRow = lipgloss.NewStyle().Border(lipgloss.HiddenBorder(), false, false, false, true)
var styleRowCurrent = lipgloss.NewStyle().Border(lipgloss.ThickBorder(), false, false, false, true)
var styleError = lipgloss.NewStyle().Foreground(lipgloss.Color("#ff0000"))

var fqnStyle = lipgloss.NewStyle().Inline(true).Bold(true)

func (m bbtfzf) View() string {
	s := m.ti.View() + "\n"

	if err := m.searchResult.err; err != nil {
		s += styleError.Render(err.Error()) + "\n"
	}

	for i, sugg := range m.searchResult.res.Targets {
		style := styleRow
		if i == m.cursor {
			style = styleRowCurrent
		}

		prefix := ""
		if sugg.Doc != "" {
			prefix = " "
			s += style.Render(sugg.Doc)
			s += "\n"
		}

		s += style.Render(prefix + fqnStyle.Render(sugg.FQN))
		s += "\n"
	}

	return s
}
