package bbt

import (
	tea "github.com/charmbracelet/bubbletea"
	"time"
)

func NewDebounce(duration time.Duration) Debounce {
	return Debounce{
		tick: time.Tick(duration),
	}
}

type Debounce struct {
	tick <-chan time.Time
}

func (d Debounce) Do(f func() tea.Msg) func() tea.Msg {
	return func() tea.Msg {
		select {
		case <-d.tick:
			return f()
		default:
			return nil
		}
	}
}
