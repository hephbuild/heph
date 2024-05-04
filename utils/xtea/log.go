package xtea

import (
	tea "github.com/charmbracelet/bubbletea"
	"github.com/hephbuild/heph/log/liblog"
	"github.com/hephbuild/heph/log/log"
)

func NewLogModel() LogModel {
	return LogModel{
		entryCh: make(chan liblog.Entry),
		done:    make(chan struct{}),
	}
}

type LogModel struct {
	entryCh chan liblog.Entry
	done    chan struct{}
}

func (m LogModel) Init() tea.Cmd {
	log.SetDiversion(m.entryCh)
	return m.Next
}

func (m LogModel) Update(msg tea.Msg) (LogModel, tea.Cmd) {
	switch msg := msg.(type) {
	case liblog.Entry:
		return m, tea.Batch(func() tea.Msg {
			lfmt := liblog.NewConsoleFormatter(log.Renderer())

			buf := lfmt.Format(msg)
			defer buf.Free()

			b := buf.Bytes()

			return tea.Println(string(b))()
		}, m.Next)
	}

	return m, nil
}

func (m LogModel) Clean() {
	close(m.done)
}

func (m LogModel) Next() tea.Msg {
	select {
	case e := <-m.entryCh:
		return e
	case <-m.done:
		return nil
	}
}
