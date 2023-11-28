package poolwait

import (
	"context"
	"github.com/hephbuild/heph/worker"
)

type keyFgWaitGroup struct{}

func ContextWithForegroundWaitGroup(ctx context.Context) (context.Context, *worker.WaitGroup) {
	deps := &worker.WaitGroup{}
	ctx = context.WithValue(ctx, keyFgWaitGroup{}, deps)

	return ctx, deps
}

// WeakWaitGroup allows to depend on other Job/Worker without keeping them in memory, allowing GC to work
type WeakWaitGroup struct {
	Deps *worker.WaitGroup
}

func (wwg *WeakWaitGroup) Add(j *worker.Job) {
	wwg.Deps.AddSem()
	go func() {
		<-j.Wait()
		wwg.Deps.DoneSem()
	}()
}

func (wwg *WeakWaitGroup) AddChild(wg *worker.WaitGroup) {
	wwg.Deps.AddSem()
	go func() {
		<-wg.Done()
		wwg.Deps.DoneSem()
	}()
}

func ForegroundWaitGroup(ctx context.Context) *WeakWaitGroup {
	if deps, ok := ctx.Value(keyFgWaitGroup{}).(*worker.WaitGroup); ok {
		return &WeakWaitGroup{Deps: deps}
	}

	return nil
}
