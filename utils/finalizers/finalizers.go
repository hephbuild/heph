package finalizers

import (
	"context"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils/flock"
	"os"
	"sync"
)

type Finalizers struct {
	m          sync.Mutex
	finalizers []func(error)
	running    bool
}

func (e *Finalizers) RegisterWithErr(f func(error)) {
	if e.running {
		return
	}

	e.m.Lock()
	defer e.m.Unlock()

	e.finalizers = append(e.finalizers, f)
}

func (e *Finalizers) Register(f func()) {
	e.RegisterWithErr(func(error) {
		f()
	})
}

func (e *Finalizers) RegisterRemove(path string) {
	e.Register(func() {
		err := os.RemoveAll(path)
		if err != nil {
			log.Error(err)
		}
	})
}

func (e *Finalizers) RegisterRemoveWithLocker(l flock.Locker, path string) {
	e.Register(func() {
		ctx := context.Background()
		ok, err := l.TryLock(ctx)
		if err != nil {
			log.Errorf("lock rm %v: %v", path, err)
			return
		}
		if !ok {
			return
		}
		defer l.Unlock()

		err = os.RemoveAll(path)
		if err != nil {
			log.Error(err)
		}
	})
}

func (e *Finalizers) Run(err error) {
	e.m.Lock()
	defer e.m.Unlock()

	e.running = true
	defer func() {
		e.running = false
	}()

	for i := len(e.finalizers) - 1; i >= 0; i-- {
		h := e.finalizers[i]
		h(err)
	}

	e.finalizers = nil
}
