package termui

import tea "github.com/charmbracelet/bubbletea"

func ChildUpdate[T interface {
	Update(msg tea.Msg) (T, tea.Cmd)
}](cmds []tea.Cmd, msg tea.Msg, m T) ([]tea.Cmd, T) {
	m, cmd := m.Update(msg)
	cmds = append(cmds, cmd)

	return cmds, m
}
