package poolwait

import (
	"context"
	"github.com/hephbuild/heph/worker2"
)

type keyFgWaitGroup struct{}

func ContextWithForegroundWaitGroup(ctx context.Context) (context.Context, *worker2.Sem) {
	deps := worker2.NewSemDep()
	ctx = context.WithValue(ctx, keyFgWaitGroup{}, deps)

	return ctx, deps
}

// GCWaitGroup allows to depend on other Job/Worker without keeping them in memory, allowing GC to work
type GCWaitGroup struct {
	Deps *worker2.Group
}

func (wwg *GCWaitGroup) AddDep(j worker2.Dep) {
	wwg.Deps.AddDep(j)
	go func() {
		<-j.Wait()
		wwg.Deps.Deps.Remove(j)
	}()
}

func ForegroundWaitGroup(ctx context.Context) *GCWaitGroup {
	if deps, ok := ctx.Value(keyFgWaitGroup{}).(*worker2.Group); ok {
		return &GCWaitGroup{Deps: deps}
	}

	return nil
}
