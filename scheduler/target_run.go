package scheduler

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/sandbox"
	"github.com/hephbuild/heph/targetrun"
)

func (e *Scheduler) RunWithSpan(ctx context.Context, rr targetrun.Request, iocfg sandbox.IOConfig) (rerr error) {
	ctx, rspan := e.Observability.SpanRun(ctx, rr.Target)
	defer rspan.EndError(rerr)

	return e.Run(ctx, rr, iocfg)
}

func (e *Scheduler) Run(ctx context.Context, rr targetrun.Request, iocfg sandbox.IOConfig) error {
	if err := ctx.Err(); err != nil {
		return err
	}

	gtarget := rr.Target

	if gtarget.Cache.Enabled && !rr.Shell && !rr.NoCache {
		if len(rr.Args) > 0 {
			return fmt.Errorf("args are not supported with cache")
		}

		cached, err := e.pullOrGetCacheAndPost(ctx, gtarget, gtarget.OutWithSupport.Names(), false, true, false)
		if err != nil {
			return err
		}

		if cached {
			return nil
		}
	}

	done := log.TraceTiming("run " + gtarget.Addr)
	defer done()

	writeableCaches, err := e.RemoteCache.WriteableCaches(ctx, gtarget)
	if err != nil {
		return fmt.Errorf("wcs: %w", err)
	}

	if !rr.Compress {
		rr.Compress = len(writeableCaches) > 0
	}

	target, err := e.Runner.Run(ctx, rr, iocfg)
	if err != nil {
		return targetrun.WrapTargetFailed(err, gtarget)
	}

	if target == nil {
		log.Debugf("target is nil after run")
		return nil
	}

	if len(writeableCaches) > 0 {
		for _, cache := range writeableCaches {
			j := e.scheduleStoreExternalCache(ctx, target.Target, cache)

			if poolDeps := ForegroundWaitGroup(ctx); poolDeps != nil {
				poolDeps.Add(j)
			}
		}
	}

	err = e.LocalCache.Post(ctx, gtarget, target.OutWithSupport.Names())
	if err != nil {
		return err
	}

	return nil
}
