package xtea

import (
	tea "github.com/charmbracelet/bubbletea"
	"github.com/hephbuild/heph/log/liblog"
	"github.com/hephbuild/heph/log/log"
	"github.com/muesli/reflow/wrap"
)

func NewLogModel() LogModel {
	return LogModel{
		entryCh: make(chan liblog.Entry),
	}
}

type LogModel struct {
	entryCh chan liblog.Entry
	width   int
}

func (m LogModel) Init() tea.Cmd {
	log.SetDiversion(m.entryCh)
	return m.Next
}

func (m LogModel) Update(msg tea.Msg) (LogModel, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		m.width = msg.Width
	case liblog.Entry:
		return m, tea.Batch(func() tea.Msg {
			lfmt := liblog.NewConsoleFormatter(log.Renderer())

			buf := lfmt.Format(msg)
			defer buf.Free()

			b := buf.Bytes()

			if m.width > 0 && len(b) > m.width {
				b = wrap.Bytes(b, m.width)
			}

			return tea.Println(string(b))()
		}, m.Next)
	}

	return m, nil
}

func (m LogModel) Next() tea.Msg {
	return <-m.entryCh
}
