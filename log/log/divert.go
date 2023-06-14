package log

import (
	"github.com/hephbuild/heph/log/liblog"
	"sync"
)

func newDivertCore(core liblog.Core) *divertCore {
	return &divertCore{
		core: core,
	}
}

type divertCore struct {
	core liblog.Core

	m      sync.Mutex
	ch     chan liblog.Entry
	chDone chan struct{}
}

func (t *divertCore) Enabled(l liblog.Level) bool {
	return t.core.Enabled(l)
}

func (t *divertCore) divert(ch chan liblog.Entry) {
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

func (t *divertCore) Log(entry liblog.Entry) error {
	t.m.Lock()
	ch, chDone := t.ch, t.chDone
	t.m.Unlock()

	if ch != nil {
		select {
		case ch <- entry:
			// Successfully diverted
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
