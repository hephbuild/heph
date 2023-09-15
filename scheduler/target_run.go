package scheduler

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/sandbox"
	"github.com/hephbuild/heph/targetrun"
	"github.com/hephbuild/heph/utils/locks"
	"github.com/hephbuild/heph/worker/poolwait"
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

	target := rr.Target

	done := log.TraceTiming("run " + target.Addr)
	defer done()

	var runLock locks.Locker
	if target.ConcurrentExecution {
		runLock = locks.NewMutex(target.Addr)
	} else {
		runLock = locks.NewFlock(target.Addr+" (run)", e.LocalCache.LockPath(target, "run"))
	}

	log.Tracef("%v locking run", target.Addr)
	err := runLock.Lock(ctx)
	if err != nil {
		return err
	}

	defer func() {
		log.Tracef("%v unlocking run", target.Addr)
		err := runLock.Unlock()
		if err != nil {
			log.Errorf("Failed to unlock %v: %v", target.Addr, err)
		}

		log.Tracef("Target DONE %v", target.Addr)
	}()

	if target.Cache.Enabled && !rr.Shell && !rr.Force {
		if len(rr.Args) > 0 {
			return fmt.Errorf("args are not supported with cache")
		}

		cached, err := e.pullOrGetCacheAndPost(ctx, target, target.OutWithSupport.Names(), false, true, false)
		if err != nil {
			return err
		}

		if cached {
			return nil
		}
	}

	writeableCaches, err := e.RemoteCache.WriteableCaches(ctx, target)
	if err != nil {
		return fmt.Errorf("wcs: %w", err)
	}

	if !rr.Compress {
		rr.Compress = len(writeableCaches) > 0
	}

	rtarget, err := e.Runner.Run(ctx, rr, iocfg)
	if err != nil {
		return targetrun.WrapTargetFailed(err, target)
	}

	if rtarget == nil {
		log.Debugf("target is nil after run")
		return nil
	}

	if len(writeableCaches) > 0 {
		for _, cache := range writeableCaches {
			j := e.scheduleStoreExternalCache(ctx, rtarget.Target, cache)

			if poolDeps := poolwait.ForegroundWaitGroup(ctx); poolDeps != nil {
				poolDeps.Add(j)
			}
		}
	}

	err = e.LocalCache.Post(ctx, target, rtarget.OutWithSupport.Names())
	if err != nil {
		return err
	}

	return nil
}
