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

// GCWaitGroup allows to depend on other Job/Worker without keeping them in memory, allowing GC to work
type GCWaitGroup struct {
	Deps *worker.WaitGroup
}

func (wwg *GCWaitGroup) Add(j *worker.Job) {
	wwg.Deps.Add(j)
	go func() {
		<-j.Wait()
		wwg.Deps.Remove(j)
	}()
}

func (wwg *GCWaitGroup) AddChild(wg *worker.WaitGroup) {
	wwg.Deps.AddChild(wg)
	go func() {
		<-wg.Done()
		wwg.Deps.RemoveChild(wg)
	}()
}

func ForegroundWaitGroup(ctx context.Context) *GCWaitGroup {
	if deps, ok := ctx.Value(keyFgWaitGroup{}).(*worker.WaitGroup); ok {
		return &GCWaitGroup{Deps: deps}
	}

	return nil
}
