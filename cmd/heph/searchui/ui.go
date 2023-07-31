package searchui

import (
	"bytes"
	"github.com/charmbracelet/bubbles/textinput"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/hephbuild/heph/bootstrap"
	"github.com/hephbuild/heph/cmd/heph/bbt"
	"github.com/hephbuild/heph/cmd/heph/search"
	"github.com/hephbuild/heph/specs"
	"strings"
	"time"
)

func TUI(targets specs.Targets, bs bootstrap.EngineBootstrap) error {
	p := tea.NewProgram(newBbtSearch(targets, bs))
	if err := p.Start(); err != nil {
		return err
	}
	return nil
}

type bbtfzf struct {
	ti           textinput.Model
	searchResult bbtSearchResult
	targets      specs.Targets
	debounce     bbt.Debounce
	search       search.Func
	deets        bool
	bs           bootstrap.EngineBootstrap
	list         *list[specs.Target]

	width, height int
}

type bbtSearchResult struct {
	query string
	res   search.Result
	err   error
}

func newBbtSearch(targets specs.Targets, bs bootstrap.EngineBootstrap) *bbtfzf {
	ti := textinput.New()
	ti.Placeholder = "Search..."
	ti.Focus()

	search, err := search.NewSearch(targets)
	if err != nil {
		panic(err)
	}

	l := &list[specs.Target]{
		render: func(sugg specs.Target, _ int, selected bool) string {
			var buf bytes.Buffer

			style := styleRow
			if selected {
				style = styleRowCurrent
			}

			prefix := ""
			if sugg.Doc != "" {
				prefix = " "
				doc := sugg.Doc
				if i := strings.Index(sugg.Doc, "\n"); i > 0 {
					doc = doc[:i]
				}
				buf.WriteString(style.Render(doc))
				buf.WriteString("\n")
			}

			buf.WriteString(style.Render(prefix + addrStyle.Render(sugg.Addr)))
			buf.WriteString("\n")

			return buf.String()
		},
		itemHeight: func(item specs.Target, index int, selected bool) int {
			if item.Doc != "" {
				return 2
			}

			return 1
		},
	}

	return &bbtfzf{
		list:     l,
		bs:       bs,
		targets:  targets,
		ti:       ti,
		search:   search,
		debounce: bbt.NewDebounce(50 * time.Millisecond),
	}
}

func (m *bbtfzf) Init() tea.Cmd {
	return tea.Batch(textinput.Blink)
}

func (m *bbtfzf) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	var cmd tea.Cmd

	switch msg := msg.(type) {
	case tea.KeyMsg:
		switch msg.Type {
		case tea.KeyCtrlC:
			return m, tea.Quit
		case tea.KeyEnter:
			m.deets = true
			cmd = tea.EnterAltScreen
		case tea.KeyEsc:
			m.deets = false
			cmd = tea.ExitAltScreen
		}
	case bbtSearchResult:
		m.searchResult = msg
		m.list.SetItems(msg.res.Targets)
	case tea.WindowSizeMsg:
		m.width, m.height = msg.Width, msg.Height
	}

	var scmd tea.Cmd
	if query := m.ti.Value(); query != m.searchResult.query {
		scmd = m.debounce.Do(func() tea.Msg {
			res, err := m.search(query, 999)
			return bbtSearchResult{res: res, err: err, query: query}
		})
	}

	if m.deets {
		m.list.height = m.height - 2 - 2 - 1
	} else {
		m.list.height = 15
	}

	var ticmd, lcmd tea.Cmd
	m.ti, ticmd = m.ti.Update(msg)
	m.list, lcmd = m.list.Update(msg)

	return m, tea.Batch(ticmd, lcmd, scmd, cmd)
}

var styleRow = lipgloss.NewStyle().Border(lipgloss.HiddenBorder(), false, false, false, true)
var styleRowCurrent = lipgloss.NewStyle().Border(lipgloss.ThickBorder(), false, false, false, true)
var styleError = lipgloss.NewStyle().Foreground(lipgloss.Color("#ff0000"))

var addrStyle = lipgloss.NewStyle().Inline(true).Bold(true)

func (m *bbtfzf) listView() string {
	var buf bytes.Buffer

	buf.WriteString(m.ti.View())
	buf.WriteString("\n")

	if err := m.searchResult.err; err != nil {
		buf.WriteString(styleError.Render(err.Error()))
		buf.WriteString("\n")
	}

	buf.WriteString(m.list.View())

	return buf.String()
}

func (m *bbtfzf) deetsView() string {
	docStyle := lipgloss.NewStyle().Padding(1, 2)
	if m.width > 0 {
		docStyle = docStyle.Width(m.width)
	}

	paneWidth := (m.width-2-2)/2 - 1

	left := lipgloss.NewStyle().Width(paneWidth)
	right := lipgloss.NewStyle().Width(paneWidth)

	doc := lipgloss.JoinHorizontal(lipgloss.Top, left.MarginRight(2).Render(m.listView()), right.Render(m.deetsRightPane(paneWidth)))

	return docStyle.Render(doc)
}

var styleTitle = lipgloss.NewStyle().Bold(true)
var styleLocation = lipgloss.NewStyle().Faint(true)

func (m *bbtfzf) deetsRightPane(width int) string {
	t, ok := m.list.get()
	if !ok {
		return ""
	}

	var sb strings.Builder

	sb.WriteString(styleTitle.Width(width).Render(t.Addr))
	sb.WriteString("\n")
	if len(t.Source) > 0 {
		sb.WriteString(styleLocation.Width(width).Render(t.SourceFile()))
		sb.WriteString("\n")
	}
	sb.WriteString("\n")

	if t.Doc != "" {
		sb.WriteString(t.Doc)
		sb.WriteString("\n")
	}

	return sb.String()
}

func (m *bbtfzf) View() string {
	if m.deets {
		return m.deetsView()
	} else {
		return m.listView()
	}
}
