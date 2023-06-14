package searchui

import (
	"bytes"
	tea "github.com/charmbracelet/bubbletea"
)

type list[T any] struct {
	render     func(item T, index int, selected bool) string
	itemHeight func(item T, index int, selected bool) int

	cursor int
	items  []T
	ritems []T
	height int
}

func (l *list[T]) Init() tea.Cmd {
	l.cursor = -1
	return nil
}

func (l *list[T]) Update(msg tea.Msg) (*list[T], tea.Cmd) {
	switch msg := msg.(type) {
	case tea.KeyMsg:
		switch msg.Type {
		case tea.KeyUp:
			if l.cursor >= 0 {
				l.cursor--
			}
		case tea.KeyDown:
			if l.cursor < len(l.items)-1 {
				l.cursor++
			}
		}
	}

	l.ritems = l.items
	rh := 0
	for i, item := range l.items {
		h := l.itemHeight(item, i, i == l.cursor)

		if rh+h <= l.height {
			rh += h
		} else {
			l.ritems = l.items[:i-1]
			break
		}
	}

	if l.cursor > len(l.ritems)-1 {
		l.cursor = len(l.ritems) - 1
	}

	return l, nil
}

func (l *list[T]) View() string {
	var buf bytes.Buffer

	for i, sugg := range l.ritems {
		buf.WriteString(l.render(sugg, i, i == l.cursor))
	}

	return buf.String()
}

func (l *list[T]) SetItems(items []T) {
	l.items = items
}

func (l *list[T]) get() (T, bool) {
	if l.cursor < 0 {
		var empty T
		return empty, false
	}

	return l.ritems[l.cursor], true
}
