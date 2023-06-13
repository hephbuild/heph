package xsync

import "sync"

type Wait struct {
	c *sync.Cond
	m sync.Mutex

	ch <-chan struct{}
}

func NewWait(m sync.Locker) *Wait {
	return &Wait{c: sync.NewCond(m)}
}

func (w *Wait) Wait(completed func() bool) <-chan struct{} {
	w.m.Lock()
	defer w.m.Unlock()

	if w.ch != nil {
		return w.ch
	}

	ch := make(chan struct{})
	w.ch = ch

	go func() {
		w.c.L.Lock()
		for !completed() {
			w.c.Wait()
		}
		close(ch)
		w.ch = nil
		w.c.L.Unlock()
	}()

	return ch
}

func (w *Wait) Broadcast() {
	w.c.Broadcast()
}

func (w *Wait) Signal() {
	w.c.Signal()
}
