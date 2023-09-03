package worker

import (
	"context"
	"github.com/hephbuild/heph/status"
	"sync"
)

func WaitGroupOr(wgs ...*WaitGroup) *WaitGroup {
	switch len(wgs) {
	case 0:
		panic("at least one WaitGroup required")
	case 1:
		return wgs[0]
	}

	doneCh := make(chan struct{})

	for _, wg := range wgs {
		wg := wg

		go func() {
			select {
			case <-doneCh:
			case <-wg.Done():
				close(doneCh)
			}
		}()
	}

	return WaitGroupChan(doneCh)
}

func WaitGroupJob(j *Job) *WaitGroup {
	wg := &WaitGroup{}
	wg.Add(j)

	return wg
}

func WaitGroupChan[T any](ch <-chan T) *WaitGroup {
	wg := &WaitGroup{}
	wg.AddSem()
	go func() {
		<-ch
		wg.DoneSem()
	}()

	return wg
}

type poolKey struct{}
type jobKey struct{}

func ContextWithPoolJob(ctx context.Context, p *Pool, j *Job) context.Context {
	ctx = context.WithValue(ctx, poolKey{}, p)
	ctx = context.WithValue(ctx, jobKey{}, j)

	return ctx
}

func PoolJobFromContext(ctx context.Context) (*Pool, *Job, bool) {
	p, _ := ctx.Value(poolKey{}).(*Pool)
	j, _ := ctx.Value(jobKey{}).(*Job)

	return p, j, p != nil
}

type dynamicStatusHandler struct {
	status.Handler
	lastStatus status.Statuser
	m          sync.Mutex
}

func (dh *dynamicStatusHandler) Set(nh status.Handler) {
	dh.m.Lock()
	defer dh.m.Unlock()

	if nh == nil && dh.Handler != nil {
		dh.Handler.Status(status.String(""))
	}
	dh.Handler = nh
	if s := dh.lastStatus; nh != nil && s != nil {
		nh.Status(dh.lastStatus)
	}
}

func (dh *dynamicStatusHandler) Status(s status.Statuser) {
	dh.m.Lock()
	defer dh.m.Unlock()

	h := dh.Handler
	if h == nil {
		h = status.DefaultHandler
	}
	dh.lastStatus = s
	h.Status(s)
}

func (dh *dynamicStatusHandler) Interactive() bool {
	h := dh.Handler
	if h == nil {
		return false
	}

	return h.Interactive()
}

func Wait(ctx context.Context, f func()) {
	p, j, ok := PoolJobFromContext(ctx)

	// We are not running in a worker, we can safely wait normally
	if !ok {
		f()
		return
	}

	// Detach from worker
	resumeCh := j.pause()

	// Wait for condition
	f()

	// Wait for it to be paused
	<-j.pausedCh

	// Reassign worker, which will resume execution by closing resumeCh
	p.jobsCh <- j

	<-resumeCh
}

func WaitWaitGroup(ctx context.Context, wg *WaitGroup) error {
	Wait(ctx, func() {
		<-wg.Done()
	})

	return wg.Err()
}
