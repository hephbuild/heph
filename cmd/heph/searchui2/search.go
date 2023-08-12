package searchui2

import (
	"context"
	"github.com/charmbracelet/bubbles/textinput"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/hephbuild/heph/cmd/heph/bbt"
	"github.com/hephbuild/heph/cmd/heph/search"
	"github.com/hephbuild/heph/specs"
	"time"
)

func newTargetSearchModel(targets specs.Targets) targetSearchModel {
	textField := textinput.New()
	textField.Placeholder = "Search..."
	textField.Focus()

	searchf, err := search.NewSearchCtx(targets)
	if err != nil {
		panic(err)
	}

	return targetSearchModel{
		textField: textField,
		search:    searchf,
		debounce:  bbt.NewDebounce(50 * time.Millisecond),
	}
}

type targetSearchModel struct {
	textField    textinput.Model
	debounce     bbt.Debounce
	search       search.FuncCtx
	searchCancel context.CancelFunc

	searchResult targetSearchResults
}

type targetSearchResults struct {
	query string
	res   search.Result
	err   error
}

func (m targetSearchModel) Init() tea.Cmd {
	return tea.Batch(textinput.Blink)
}

func (m targetSearchModel) Update(msg tea.Msg) (targetSearchModel, tea.Cmd) {
	var (
		cmds []tea.Cmd
		cmd  tea.Cmd
	)

	switch msg := msg.(type) {
	case targetSearchResults:
		m.searchResult = msg
	}

	if query := m.textField.Value(); query != m.searchResult.query {
		cmd = m.debounce.Do(func() tea.Msg {
			res, err := m.search(context.Background(), query, 200)
			return targetSearchResults{res: res, err: err, query: query}
		})
		cmds = append(cmds, cmd)
	}

	m.textField, cmd = m.textField.Update(msg)
	cmds = append(cmds, cmd)

	return m, tea.Batch(cmds...)
}

func (m targetSearchModel) View() string {
	return m.textField.View()
}

func (m targetSearchModel) SetWidth(width int) {
	m.textField.Width = width
}
