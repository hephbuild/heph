package searchui2

import (
	"github.com/charmbracelet/bubbles/help"
	"github.com/charmbracelet/bubbles/key"
	"github.com/charmbracelet/bubbles/list"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/xreflow"
	"strings"
)

type model struct {
	ide           bool
	width, height int
	colWidth      int

	search  targetSearchModel
	results targetSearchResults
	list    list.Model
	help    help.Model
	keys    keyMap

	run targetRunModel

	running bool
}

func (m model) Init() tea.Cmd {
	return tea.Batch(m.search.Init())
}

type ideMsg struct {
	enable bool
}

func ideMode(v bool) tea.Cmd {
	return func() tea.Msg {
		return ideMsg{enable: v}
	}
}

type recalculateMsg struct{}

var terminalStyle = lipgloss.NewStyle().Border(lipgloss.NormalBorder(), true, false)

func (m model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	var (
		cmd  tea.Cmd
		cmds []tea.Cmd
	)

	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		m.width, m.height = msg.Width, msg.Height

		return m, func() tea.Msg {
			return recalculateMsg{}
		}
	case recalculateMsg:
		m.help.Width = m.width

		if m.ide {
			m.colWidth = m.width/2 - 2

			m.search.SetWidth(m.colWidth)
			m.run.SetSize(m.colWidth, (m.height-1)/2)

			// remove:
			// - help
			// - search
			// - border
			// - pagination
			m.list.SetSize(m.colWidth, m.height-1-1-2-1)
		} else {
			m.list.SetSize(m.width, plainResults+1)
			m.search.SetWidth(m.width)
			m.run.SetSize(m.width, 23)
		}
	case ideMsg:
		m.ide = msg.enable
		if msg.enable {
			cmds = append(cmds, tea.Sequence(
				tea.EnterAltScreen,
				func() tea.Msg {
					return recalculateMsg{}
				},
			))
		} else {
			cmds = append(cmds, tea.Sequence(
				tea.ExitAltScreen,
				func() tea.Msg {
					return recalculateMsg{}
				},
			))
		}
	case tea.KeyMsg:
		switch {
		case key.Matches(msg, m.keys.Run):
			t := m.selectedTarget()

			if t != nil {
				return m, func() tea.Msg {
					return targetRunStartMsg{
						target: t.Addr,
					}
				}
			}

		case key.Matches(msg, m.keys.Esc):
			if m.ide {
				if !m.running && m.run.View() != "" {
					return m, func() tea.Msg {
						return targetRunReset{}
					}
				}

				return m, ideMode(false)
			}
		case key.Matches(msg, m.keys.Details):
			if m.ide {
				return m, ideMode(false)
			} else {
				return m, ideMode(true)
			}
		case key.Matches(msg, m.keys.Quit):
			if m.running {
				return m, func() tea.Msg {
					return targetRunKill{}
				}
			}
			return m, tea.Sequence(ideMode(false), tea.Quit)
		case key.Matches(msg, m.keys.Help):
			m.help.ShowAll = !m.help.ShowAll
			k := m.keys.Help.Keys()[0]
			if m.help.ShowAll {
				m.keys.Help.SetHelp(k, "less")
			} else {
				m.keys.Help.SetHelp(k, "more")
			}

			return m, nil
		}
	case targetSearchResults:
		m.results = msg

		items := ads.Map(m.results.res.Targets, func(t specs.Target) list.Item {
			return item{t: t}
		})
		if !m.ide && len(items) > plainResults {
			items = items[:plainResults]
		}

		m.list.SetItems(items)
	case targetRunStartMsg:
		m.running = true
	case targetRunDone:
		m.running = false

		if m.ide {
			// TODO: tea.Println doesnt show up in alt screen mode
		} else {
			var scmds []tea.Cmd
			if msg.snapshot != "" {
				scmds = append(scmds, tea.Println(terminalStyle.Render(strings.TrimSpace(msg.snapshot))))
			}

			if msg.err != nil {
				scmds = append(scmds, tea.Println("RUN ERR ", msg.err.Error()))
			} else {
				scmds = append(scmds, tea.Println("Done."))
			}

			cmds = append(cmds, tea.Sequence(scmds...))
		}
	}

	m.search, cmd = m.search.Update(msg)
	cmds = append(cmds, cmd)

	m.list, cmd = m.list.Update(msg)
	cmds = append(cmds, cmd)

	m.run, cmd = m.run.Update(msg)
	cmds = append(cmds, cmd)

	return m, tea.Batch(cmds...)
}

var styleTitle = lipgloss.NewStyle().Bold(true)
var styleLocation = lipgloss.NewStyle().Faint(true)

func (m model) selectedTarget() *specs.Target {
	i := m.list.Index()
	if i < 0 {
		return nil
	}

	targets := m.results.res.Targets

	if i >= len(targets) {
		return nil
	}

	t := targets[i]

	return &t
}

func (m model) details() string {
	t := m.selectedTarget()
	if t == nil {
		return ""
	}

	var sb strings.Builder

	sb.WriteString(styleTitle.Render(t.Addr))
	sb.WriteString("\n")
	if len(t.Source) > 0 {
		sb.WriteString(styleLocation.Render(t.SourceFile()))
		sb.WriteString("\n")
	}
	sb.WriteString("\n")

	if t.Doc != "" {
		sb.WriteString(t.Doc)
		sb.WriteString("\n")
	}

	return xreflow.WrapString(sb.String(), m.colWidth)
}

func (m model) View() string {
	if !m.ide {
		if m.running {
			return terminalStyle.Render(m.run.View())
		}

		components := make([]string, 0)
		components = append(components, m.search.View())

		if m.results.err != nil {
			components = append(components, m.results.err.Error())
		}

		components = append(components,
			m.list.View(),
			m.help.View(m.keys),
		)

		return lipgloss.JoinVertical(
			lipgloss.Top,
			components...,
		)
	}

	col := lipgloss.NewStyle().
		Width(m.colWidth).
		Height(m.height-1-2). // height - help - border
		Border(lipgloss.NormalBorder(), true)

	return lipgloss.JoinVertical(
		lipgloss.Top,
		lipgloss.PlaceVertical(
			m.height-1, lipgloss.Top,
			lipgloss.JoinHorizontal(
				lipgloss.Center,
				col.Render(lipgloss.JoinVertical(
					lipgloss.Top,
					m.search.View(),
					m.list.View(),
				)),
				col.Render(lipgloss.JoinVertical(
					lipgloss.Top,
					lipgloss.NewStyle().Height(col.GetHeight()-lipgloss.Height(m.run.View())).Render(m.details()),
					m.run.View(),
				)),
			),
		),
		m.help.View(m.keys),
	)
}
