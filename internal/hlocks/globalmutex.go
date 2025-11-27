package hlocks

import (
	"context"
	"sync"
	"time"

	sync_map "github.com/zolstein/sync-map"
)

var globalmutex = sync_map.Map[string, RWLocker]{}
var globalmutexgc sync.Mutex

func gcGlobalMutex() {
	globalmutexgc.Lock()
	defer globalmutexgc.Unlock()

	globalmutex.Range(func(key string, value RWLocker) bool {
		ok, _ := value.TryLock(context.Background())
		if !ok {
			return true
		}

		globalmutex.Delete(key)

		_ = value.Unlock()

		return true
	})
}

var globalMutexGcStopCh chan struct{}

func StartGlobalMutexGC() {
	if globalMutexGcStopCh != nil {
		panic("global mutex GC already started")
	}

	globalMutexGcStopCh = make(chan struct{})

	go runGlobalMutexGC(10*time.Second, globalMutexGcStopCh)
}

func StopGlobalMutexGC() {
	if globalMutexGcStopCh == nil {
		panic("global mutex GC already stopped")
	}

	close(globalMutexGcStopCh)
	globalMutexGcStopCh = nil
}

func runGlobalMutexGC(interval time.Duration, stopCh <-chan struct{}) {
	t := time.NewTicker(interval)
	defer t.Stop()

	for {
		select {
		case <-t.C:
			gcGlobalMutex()
		case <-stopCh:
			return
		}
	}
}

func NewGlobalMutex(id string) RWLocker {
	globalmutexgc.Lock()
	defer globalmutexgc.Unlock()

	l, ok := globalmutex.Load(id)
	if !ok {
		l, _ = globalmutex.LoadOrStore(id, NewMutex(id))
	}

	return globalMutexLocker{
		RWLocker: l,
	}
}

type globalMutexLocker struct {
	RWLocker
}
