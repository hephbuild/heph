package termui

import tea "charm.land/bubbletea/v2"

func ChildUpdate[T interface {
	Update(msg tea.Msg) (T, tea.Cmd)
}](cmds []tea.Cmd, msg tea.Msg, m T) ([]tea.Cmd, T) {
	m, cmd := m.Update(msg)
	if cmd != nil {
		cmds = append(cmds, cmd)
	}

	return cmds, m
}
