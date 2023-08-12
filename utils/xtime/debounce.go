package xtime

import (
	"context"
	"sync"
	"time"
)

func NewDebounce(duration time.Duration) *Debounce {
	t := time.NewTimer(duration)
	t.Stop()

	return &Debounce{
		d: duration,
		t: t,
	}
}

type Debounce struct {
	cancel context.CancelFunc
	m      sync.Mutex
	rm     sync.Mutex
	t      *time.Timer
	f      func(ctx context.Context)
	d      time.Duration
}

func (d *Debounce) Do(f func(ctx context.Context)) {
	d.m.Lock()
	defer d.m.Unlock()

	if d.f != nil {
		d.t.Reset(d.d)
	} else {
		go func() {
			<-d.t.C

			if cancel := d.cancel; cancel != nil {
				cancel()
			}

			d.rm.Lock()
			defer d.rm.Unlock()

			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()

			d.cancel = cancel

			d.f(ctx)

			d.f = nil
		}()
	}

	d.f = f
}
