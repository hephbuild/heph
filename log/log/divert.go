package log

import (
	"github.com/charmbracelet/lipgloss"
	"github.com/hephbuild/heph/log/liblog"
	"github.com/muesli/reflow/wrap"
	"sync"
)

func newDivertCore(r *lipgloss.Renderer, core liblog.Core) *divertCore {
	return &divertCore{
		fmt:  liblog.NewConsoleFormatter(r),
		core: core,
	}
}

type divertCore struct {
	fmt  *liblog.ConsoleFormatter
	core liblog.Core

	m      sync.Mutex
	ch     chan FormattableEntry
	chDone chan struct{}
}

func (t *divertCore) Enabled(l liblog.Level) bool {
	return t.core.Enabled(l)
}

func (t *divertCore) divert(ch chan FormattableEntry) {
	t.m.Lock()
	defer t.m.Unlock()

	t.ch = ch

	if t.chDone != nil {
		close(t.chDone)
		t.chDone = nil
	}
	if ch != nil {
		t.chDone = make(chan struct{})
	}
}

type FormattableEntry struct {
	Entry liblog.Entry

	fmt *liblog.ConsoleFormatter
}

func (f FormattableEntry) Format(termWidth int) string {
	buf := f.fmt.Format(f.Entry)
	defer buf.Free()

	b := buf.Bytes()

	if termWidth > 0 && len(b) > termWidth {
		b = wrap.Bytes(b, termWidth)
	}

	return string(b)
}

func (t *divertCore) Log(entry liblog.Entry) error {
	t.m.Lock()
	ch, chDone := t.ch, t.chDone
	t.m.Unlock()

	if ch != nil {
		select {
		case ch <- FormattableEntry{
			Entry: entry,
			fmt:   t.fmt,
		}:
			return nil
		case <-chDone:
			// Another diversion is in place, try again
			if t.ch != nil {
				return t.Log(entry)
			}

			t.m.Lock()
			defer t.m.Unlock()

			// Diversion over, continue as usual...
		}
	}

	return t.core.Log(entry)
}

var _ liblog.Core = (*divertCore)(nil)
