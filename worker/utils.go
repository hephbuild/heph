package worker

import (
	"context"
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

func Suspend(ctx context.Context, f func()) {
	p, j, ok := PoolJobFromContext(ctx)

	// We are not running in a worker, we can safely wait normally
	if !ok {
		f()
		return
	}

	// Detach from worker
	resumeCh := j.suspend()

	// Wait for it to be suspended
	<-j.suspendedCh

	// Wait for condition
	f()

	// Reassign worker, which will resume execution by closing resumeCh
	p.jobsCh <- j

	<-resumeCh
}

func SuspendE(ctx context.Context, f func() error) error {
	var err error
	Suspend(ctx, func() {
		err = f()
	})
	return err
}

func SuspendWaitGroup(ctx context.Context, wg *WaitGroup) error {
	return SuspendE(ctx, func() error {
		<-wg.Done()
		return wg.Err()
	})
}
