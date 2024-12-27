package hbbtch

import tea "github.com/charmbracelet/bubbletea"

type Model[T any] struct {
	ch    chan container[T]
	onMsg func(T) tea.Cmd
}

type container[T any] struct {
	v      T
	doneCh chan struct{}
}

func (m Model[T]) next() tea.Cmd {
	return func() tea.Msg {
		return <-m.ch
	}
}

func (m Model[T]) Init() tea.Cmd {
	return m.next()
}

func (m Model[T]) Update(msg tea.Msg) (Model[T], tea.Cmd) {
	switch msg := msg.(type) {
	case container[T]:
		defer close(msg.doneCh)

		cmd := m.onMsg(msg.v)
		return m, tea.Batch(cmd, m.next())
	}

	return m, nil
}

func (m Model[T]) View() string {
	return ""
}

func (m Model[T]) Send(v T) <-chan struct{} {
	doneCh := make(chan struct{})
	m.ch <- container[T]{v: v, doneCh: doneCh}
	return doneCh
}

func New[T any](onMsg func(T) tea.Cmd) Model[T] {
	return Model[T]{ch: make(chan container[T]), onMsg: onMsg}
}
