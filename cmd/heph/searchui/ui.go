package searchui

import (
	"bytes"
	"github.com/charmbracelet/bubbles/textinput"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/hephbuild/heph/bootstrap"
	"github.com/hephbuild/heph/cmd/heph/bbt"
	"github.com/hephbuild/heph/cmd/heph/search"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/targetspec"
	"golang.org/x/term"
	"os"
	"time"
)

func TUI(targets targetspec.TargetSpecs, bs bootstrap.EngineBootstrap) error {
	p := tea.NewProgram(newBbtSearch(targets, bs))
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
	search       search.Func
	deets        bool
	deetsTarget  *graph.Target
	bs           bootstrap.EngineBootstrap
}

type bbtSearchResult struct {
	query string
	res   search.Result
	err   error
}

func newBbtSearch(targets targetspec.TargetSpecs, bs bootstrap.EngineBootstrap) *bbtfzf {
	ti := textinput.New()
	ti.Placeholder = "Search..."
	ti.Focus()

	search, err := search.NewSearch(targets)
	if err != nil {
		panic(err)
	}

	return &bbtfzf{
		bs:       bs,
		targets:  targets,
		ti:       ti,
		cursor:   -1,
		search:   search,
		debounce: bbt.NewDebounce(50 * time.Millisecond),
	}
}

func (m *bbtfzf) Init() tea.Cmd {
	return textinput.Blink
}

func (m *bbtfzf) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	var cmd tea.Cmd

	switch msg := msg.(type) {
	case tea.KeyMsg:
		switch msg.Type {
		case tea.KeyCtrlC:
			return m, tea.Quit
		case tea.KeyEnter:
			return m, func() tea.Msg {
				m.deets = true
				return tea.EnterAltScreen()
			}
		case tea.KeyEsc:
			return m, func() tea.Msg {
				m.deets = false
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

func (m *bbtfzf) listView() string {
	var buf bytes.Buffer

	buf.WriteString(m.ti.View())
	buf.WriteString("\n")

	if err := m.searchResult.err; err != nil {
		buf.WriteString(styleError.Render(err.Error()))
		buf.WriteString("\n")
	}

	for i, sugg := range m.searchResult.res.Targets {
		style := styleRow
		if i == m.cursor {
			style = styleRowCurrent
		}

		prefix := ""
		if sugg.Doc != "" {
			prefix = " "
			buf.WriteString(style.Render(sugg.Doc))
			buf.WriteString("\n")
		}

		buf.WriteString(style.Render(prefix + fqnStyle.Render(sugg.FQN)))
		buf.WriteString("\n")
	}

	return buf.String()
}

func (m *bbtfzf) deetsView() string {
	physicalWidth, _, _ := term.GetSize(int(os.Stderr.Fd()))

	docStyle := lipgloss.NewStyle().Padding(1, 2, 1, 2)

	if physicalWidth > 0 {
		docStyle = docStyle.Width(physicalWidth)
	}

	left := lipgloss.NewStyle().Width(physicalWidth / 2)
	right := lipgloss.NewStyle().Width(physicalWidth / 2)

	doc := lipgloss.JoinHorizontal(lipgloss.Top, left.Render(m.listView()), right.Render("right"))

	return docStyle.Render(doc)
}

func (m *bbtfzf) View() string {
	if m.deets {
		return m.deetsView()
	} else {
		return m.listView()
	}
}
