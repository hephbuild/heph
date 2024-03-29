package xtea

import (
	tea "github.com/charmbracelet/bubbletea"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils/xpanic"
)

func RunModel(model tea.Model, opts ...tea.ProgramOption) error {
	p := New(model, opts...)

	return Run(p)
}

func Run(p *tea.Program) error {
	defer func() {
		log.SetDiversion(nil)
		_ = xpanic.Recover(func() error {
			return p.ReleaseTerminal()
		})
	}()

	_, err := p.Run()
	return err
}

func New(model tea.Model, opts ...tea.ProgramOption) *tea.Program {
	return tea.NewProgram(model, append(opts, tea.WithOutput(log.Writer()), tea.WithoutSignalHandler())...)
}
