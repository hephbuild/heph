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

func ForegroundWaitGroup(ctx context.Context) *worker.WaitGroup {
	if deps, ok := ctx.Value(keyFgWaitGroup{}).(*worker.WaitGroup); ok {
		return deps
	}

	return nil
}
