package xcontext

import (
	"context"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils/ads"
	"github.com/hephbuild/heph/utils/xsync"
	"os"
	"os/signal"
	"sync"
	"syscall"
	"time"
)

func IsDone(ctx context.Context) bool {
	select {
	case <-ctx.Done():
		return true
	default:
		return false
	}
}

type entry struct {
	softCancel context.CancelFunc
	hardCancel context.CancelFunc
}

type state struct {
	ctxs []*entry
	m    sync.Mutex
	w    *xsync.Wait
}

func newSoftCancelState() *state {
	s := &state{}
	s.w = xsync.NewWait(&s.m)

	return s
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

	a.add(e)

	return scctx, hctx, func() {
		e.softCancel()
		e.hardCancel()

		a.remove(e)
	}
}

func (a *state) wait() <-chan struct{} {
	return a.w.Wait(func() bool {
		return len(a.ctxs) == 0
	})
}

func (a *state) add(e *entry) {
	a.m.Lock()
	defer a.m.Unlock()

	a.ctxs = append(a.ctxs, e)
}

func (a *state) remove(e *entry) {
	a.m.Lock()
	defer a.m.Unlock()

	a.ctxs = ads.Remove(a.ctxs, e)

	a.w.Broadcast()
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

type keyCancel struct{}

func Cancel(ctx context.Context) {
	cancel := ctx.Value(keyCancel{}).(context.CancelFunc)

	cancel()
}

func BootstrapSoftCancel() (context.Context, context.CancelFunc) {
	ctx, cancel := context.WithCancel(context.Background())

	sigCh := make(chan os.Signal)
	signal.Notify(sigCh, os.Interrupt, syscall.SIGTERM)

	sc := newSoftCancelState()

	go func() {
		<-sigCh
		cancel()
		if sc.has() {
			hardCanceled := false
			go func() {
				<-time.After(200 * time.Millisecond)
				if hardCanceled {
					return
				}
				log.Warnf("Attempting to cancel... ctrl+c one more time to force")
			}()
			select {
			case <-sigCh:
			}
			log.Warnf("Forcing cancellation...")
			hardCanceled = true
			sc.hardCancel()
			select {
			// Wait for soft cancel to all be unregistered, should be fast, unless something is stuck
			case <-sc.wait():
				// Wait for graceful exit
				<-time.After(2 * time.Second)
			case <-time.After(2 * time.Second):
				// All soft cancel did not unregister, something is stuck...
			}
		} else {
			<-time.After(2 * time.Second)
		}

		log.Error("Something seems to be stuck, ctrl+c one more time to forcefully exit")
		sig := <-sigCh
		sigN := 0
		if sig, ok := sig.(syscall.Signal); ok {
			sigN = int(sig)
		}
		os.Exit(128 + sigN)
	}()

	ctx = context.WithValue(ctx, keySoftCancelState{}, sc)
	ctx = context.WithValue(ctx, keyCancel{}, context.CancelFunc(func() {
		sigCh <- os.Interrupt
	}))

	return ctx, func() {
		cancel()
		sc.hardCancel()
	}
}
