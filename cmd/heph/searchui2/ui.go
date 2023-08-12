package searchui2

import (
	"github.com/charmbracelet/bubbles/help"
	"github.com/charmbracelet/bubbles/list"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/hephbuild/heph/bootstrap"
	"github.com/hephbuild/heph/specs"
)

const plainResults = 15

func New(targets specs.Targets, bs bootstrap.EngineBootstrap) tea.Model {
	h := help.New()

	l := list.New(nil, itemDelegate{}, 0, plainResults+1)
	l.SetShowTitle(false)
	l.SetShowHelp(false)
	l.SetShowStatusBar(false)
	l.SetFilteringEnabled(false)
	l.DisableQuitKeybindings()

	keys := defaultKeys()
	keys.list = l.KeyMap

	return model{
		search: newTargetSearchModel(targets),
		list:   l,
		help:   h,
		keys:   keys,
	}
}
