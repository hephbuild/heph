package hlocks

import (
	"context"
	"errors"
	"go.opentelemetry.io/otel"
)

var tmutexTracer = otel.Tracer("heph/internal/hlocks/tmutex")

func NewT(outer Locker, inner RWLocker) *TMutex {
	return &TMutex{
		outer: outer,
		inner: inner,
	}
}

var _ Locker = (*TMutex)(nil)
var _ RWLocker = (*TMutex)(nil)

type TMutex struct {
	// Readlocks need only acquire the inner mutex. Writelocks must first
	// acquire the outer mutex and then the inner mutex. This means that only
	// one writelock has access to the inner mutex at a time. Upon being
	// demoted, a writelock releases the inner mutex, which allows readlocks to
	// acquire the mutex. The outer lock remains locked, preventing other lock
	// calls from blocking access to the inner mutex.
	outer Locker
	inner RWLocker
}

func (tm *TMutex) TryRLock(ctx context.Context) (bool, error) {
	ctx, span := tmutexTracer.Start(ctx, "TryRLock")
	defer span.End()

	return tm.inner.TryRLock(ctx)
}

func (tm *TMutex) TryLock(ctx context.Context) (bool, error) {
	ctx, span := tmutexTracer.Start(ctx, "TryLock")
	defer span.End()

	ok, err := tm.outer.TryLock(ctx)
	if !ok || err != nil {
		return false, err
	}
	ok, err = tm.inner.TryLock(ctx)
	if !ok || err != nil {
		uerr := tm.outer.Unlock()
		if uerr != nil {
			err = errors.Join(err, uerr)
		}

		return false, err
	}

	return true, nil
}

func (tm *TMutex) Clean() error {
	err1 := tm.inner.Clean()
	err2 := tm.outer.Clean()

	return errors.Join(err1, err2)
}

func (tm *TMutex) RLock2Lock(ctx context.Context) error {
	ctx, span := tmutexTracer.Start(ctx, "RLock2Lock")
	defer span.End()

	err := tm.outer.Lock(ctx)
	if err != nil {
		return err
	}
	err = tm.inner.RUnlock()
	if err != nil {
		return err
	}
	err = tm.inner.Lock(ctx)
	if err != nil {
		return err
	}

	return nil
}

func (tm *TMutex) Lock2RLock(ctx context.Context) error {
	ctx, span := tmutexTracer.Start(ctx, "Lock2RLock")
	defer span.End()

	err := tm.inner.Unlock()
	if err != nil {
		return err
	}
	err = tm.inner.RLock(ctx)
	if err != nil {
		return err
	}
	err = tm.outer.Unlock()
	if err != nil {
		return err
	}

	return nil
}

func (tm *TMutex) RLock(ctx context.Context) error {
	ctx, span := tmutexTracer.Start(ctx, "RLock")
	defer span.End()

	return tm.inner.RLock(ctx)
}

func (tm *TMutex) RUnlock() error {
	return tm.inner.RUnlock()
}

func (tm *TMutex) Lock(ctx context.Context) error {
	ctx, span := tmutexTracer.Start(ctx, "Lock")
	defer span.End()

	// The outer lock must be acquired first, and then the inner lock acquired
	// second. This ensures that only one writelock has access to the inner
	// lock at a time.
	err := tm.outer.Lock(ctx)
	if err != nil {
		return err
	}
	err = tm.inner.Lock(ctx)
	if err != nil {
		uerr := tm.outer.Unlock()
		if uerr != nil {
			err = errors.Join(err, uerr)
		}

		return err
	}

	return nil
}

func (tm *TMutex) Unlock() error {
	err1 := tm.inner.Unlock()
	err2 := tm.outer.Unlock()

	return errors.Join(err1, err2)
}
