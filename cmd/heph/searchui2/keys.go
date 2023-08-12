package searchui2

import (
	"github.com/charmbracelet/bubbles/key"
	"github.com/charmbracelet/bubbles/list"
)

type keyMap struct {
	Quit    key.Binding
	Help    key.Binding
	Run     key.Binding
	Details key.Binding
	Esc     key.Binding

	list list.KeyMap
}

func defaultKeys() keyMap {
	return keyMap{
		Quit: key.NewBinding(
			key.WithKeys("ctrl+c"),
			key.WithHelp("ctrl+c", "quit"),
		),
		Help: key.NewBinding(
			key.WithKeys("ctrl+h"),
			key.WithHelp("ctrl+h", "more"),
		),
		Run: key.NewBinding(
			key.WithKeys("enter"),
			key.WithHelp("enter", "run"),
		),
		Details: key.NewBinding(
			key.WithKeys("?"),
			key.WithHelp("?", "details"),
		),
		Esc: key.NewBinding(
			key.WithKeys("esc"),
		),
	}
}

// ShortHelp returns keybindings to be shown in the mini help view. It's part
// of the key.Map interface.
func (k keyMap) ShortHelp() []key.Binding {
	return []key.Binding{k.list.CursorUp, k.list.CursorDown, k.Details, k.Run, k.Help, k.Quit}
}

// FullHelp returns keybindings for the expanded help view. It's part of the
// key.Map interface.
func (k keyMap) FullHelp() [][]key.Binding {
	return [][]key.Binding{
		{
			k.list.CursorUp,
			k.list.CursorDown,
			k.list.NextPage,
			k.list.PrevPage,
			k.list.GoToStart,
			k.list.GoToEnd,
		},
		{k.Details, k.Run},
		{k.Quit, k.Help},
	}
}
