package hbbtlog

import (
	"context"
	"log/slog"
	"os"
	"sync"
	"sync/atomic"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/hephbuild/hephv2/internal/hbbt/hbbtch"
	"github.com/hephbuild/hephv2/internal/hcore/hlog"
)

type Hijacker struct {
	hbbtch.Model[slog.Record]
	*hijackerData
}

type hijackerData struct {
	mode atomic.Value
	cond *sync.Cond
}

func (h Hijacker) GetMode() LogHijackerMode {
	return h.mode.Load().(LogHijackerMode) //nolint:errcheck
}

func (h Hijacker) GetModeWait() LogHijackerMode {
	mode := h.GetMode()

	if mode == LogHijackerModeWait {
		h.cond.L.Lock()
		for {
			mode = h.GetMode()
			if mode != LogHijackerModeWait {
				break
			}
			h.cond.Wait()
		}
		h.cond.L.Unlock()
	}

	return mode
}

func (h Hijacker) SetMode(mode LogHijackerMode) {
	h.cond.L.Lock()
	h.mode.Store(mode)
	h.cond.L.Unlock()

	h.cond.Broadcast()
}

func (h Hijacker) Init() tea.Cmd {
	h.mode.Store(LogHijackerModeHijack)

	return h.Model.Init()
}

func (h Hijacker) Update(msg tea.Msg) (Hijacker, tea.Cmd) {
	var cmd tea.Cmd
	h.Model, cmd = h.Model.Update(msg)

	return h, cmd
}

func (h Hijacker) Handler(next hlog.HandleFunc, ctx context.Context, record slog.Record) error {
	mode := h.GetModeWait()

	switch mode {
	case LogHijackerModeWait:
		// will not happen
	case LogHijackerModeDisabled:
		return next(ctx, record)
	case LogHijackerModeHijack:
		h.Send(record.Clone())
	}

	return nil
}

type LogHijackerMode int

const (
	LogHijackerModeDisabled LogHijackerMode = iota
	LogHijackerModeWait
	LogHijackerModeHijack
)

func NewLogHijacker() Hijacker {
	renderer := hlog.NewRenderer(os.Stderr)

	h := Hijacker{
		Model: hbbtch.New[slog.Record](func(record slog.Record) tea.Cmd {
			return tea.Println(hlog.FormatRecord(renderer, record))
		}),
		hijackerData: &hijackerData{
			cond: sync.NewCond(&sync.Mutex{}),
		},
	}
	h.mode.Store(LogHijackerModeWait)

	return h
}
