package searchui2

import (
	tea "github.com/charmbracelet/bubbletea"
	"github.com/hephbuild/heph/utils/xvt10x"
	"os"
	"os/exec"
	"time"
)

type targetRunModel struct {
	t       xvt10x.Terminal
	v       string
	running bool
	cmd     *exec.Cmd
	width   int
	height  int
}

type targetRunStartMsg struct {
	target string
}

type targetRunDone struct {
	err      error
	snapshot string
}

type targetRunKill struct{}
type targetRunReset struct{}

type tickMsg time.Time

func doTick() tea.Cmd {
	return tea.Tick(100*time.Millisecond, func(t time.Time) tea.Msg {
		return tickMsg(t)
	})
}

func (m targetRunModel) Update(msg tea.Msg) (targetRunModel, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.QuitMsg:
		_ = m.t.Close()
		return m, nil
	case targetRunStartMsg:
		_ = m.t.Close()

		exe, _ := os.Executable()
		cmd := exec.Command(exe, "run", msg.target)

		t, err := xvt10x.NewCmd(cmd)
		if err != nil {
			return m, func() tea.Msg {
				return targetRunDone{err, ""}
			}
		}

		t.Resize(uint16(m.width), uint16(m.height))

		m.t = t
		m.running = true
		m.cmd = cmd

		return m, tea.Batch(
			func() tea.Msg {
				err := cmd.Run()
				if err != nil {
					return targetRunDone{err, m.t.Render(nil)}
				}

				return targetRunDone{err, m.t.Render(nil)}
			},
			doTick(),
		)
	case tickMsg:
		m.v = m.t.Render(nil)
		if !m.running {
			return m, nil
		}
		return m, doTick()
	case targetRunDone:
		m.running = false
	case targetRunKill:
		if m.cmd != nil && m.cmd.Process != nil {
			_ = m.cmd.Process.Kill()
		}
	case targetRunReset:
		m.v = ""
	}

	return m, nil
}

func (m targetRunModel) View() string {
	return m.v
}

func (m *targetRunModel) SetSize(width, height int) {
	m.width, m.height = width, height
	m.t.Resize(uint16(width), uint16(height))
}
