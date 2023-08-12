package searchui2

import (
	"context"
	"github.com/charmbracelet/bubbles/textinput"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/hephbuild/heph/cmd/heph/search"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/xtime"
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
		debounce:  xtime.NewDebounce(10 * time.Millisecond),
	}
}

type targetSearchModel struct {
	textField    textinput.Model
	debounce     *xtime.Debounce
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
		ch := make(chan targetSearchResults)

		m.debounce.Do(func(ctx context.Context) {
			res, err := m.search(ctx, query, 200)
			ch <- targetSearchResults{res: res, err: err, query: query}
		})

		cmds = append(cmds, func() tea.Msg {
			msg, ok := <-ch
			if ok {
				return msg
			}
			return nil
		})
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
