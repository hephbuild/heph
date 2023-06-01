package xcontext

import (
	"context"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils/ads"
	"os"
	"os/signal"
	"sync"
	"syscall"
	"time"
)

type entry struct {
	softCancel context.CancelFunc
	hardCancel context.CancelFunc
}

type state struct {
	ctxs []*entry
	m    sync.Mutex
}

// New returns one context that will be canceled by soft cancel first, the second one will act as a force cancel
// both inherit values from their parents
func (a *state) New(parent context.Context) (context.Context, context.Context, func()) {
	scctx, scancel := context.WithCancel(parent)
	hcctx, hcancel := context.WithCancel(context.Background())

	hctx := CancellableContext{
		Parent: parent,
		Cancel: hcctx,
	}

	e := &entry{
		softCancel: scancel,
		hardCancel: hcancel,
	}

	a.m.Lock()
	a.ctxs = append(a.ctxs, e)
	a.m.Unlock()

	return scctx, hctx, func() {
		e.softCancel()
		e.hardCancel()

		a.m.Lock()
		a.ctxs = ads.Remove(a.ctxs, e)
		a.m.Unlock()
	}
}

func (a *state) has() bool {
	a.m.Lock()
	defer a.m.Unlock()

	return len(a.ctxs) > 0
}

func (a *state) hardCancel() bool {
	a.m.Lock()
	defer a.m.Unlock()

	if len(a.ctxs) == 0 {
		return false
	}

	for _, e := range a.ctxs {
		e.hardCancel()
	}

	return true
}

type keySoftCancelState struct{}

// NewSoftCancel See softCancel.New
func NewSoftCancel(parent context.Context) (context.Context, context.Context, context.CancelFunc) {
	sc := parent.Value(keySoftCancelState{}).(*state)

	return sc.New(parent)
}

type CancellableContext struct {
	// Parent is used for Value()
	Parent context.Context
	// Cancel is used to inherit cancellation
	Cancel context.Context
}

func (c CancellableContext) Deadline() (time.Time, bool)       { return c.Cancel.Deadline() }
func (c CancellableContext) Done() <-chan struct{}             { return c.Cancel.Done() }
func (c CancellableContext) Err() error                        { return c.Cancel.Err() }
func (c CancellableContext) Value(key interface{}) interface{} { return c.Parent.Value(key) }

func BootstrapSoftCancel() (context.Context, context.CancelFunc) {
	ctx, cancel := context.WithCancel(context.Background())

	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, os.Interrupt, syscall.SIGTERM)

	sc := &state{}

	go func() {
		<-sigCh
		cancel()
		if sc.has() {
			hardCanceled := false
			go func() {
				<-time.After(time.Second)
				if hardCanceled {
					return
				}
				log.Warnf("Attempting to soft cancel... ctrl+c one more time to force")
			}()
			select {
			case <-sigCh:
			case <-time.After(30 * time.Second):
			}
			hardCanceled = true
			sc.hardCancel()
		}

		<-time.After(2 * time.Second)
		log.Error("Something seems to be stuck, ctrl+c one more time to forcefully exit")
		sig := <-sigCh
		sigN := 0
		if sig, ok := sig.(syscall.Signal); ok {
			sigN = int(sig)
		}
		os.Exit(128 + sigN)
	}()

	ctx = context.WithValue(ctx, keySoftCancelState{}, sc)

	return ctx, cancel
}
