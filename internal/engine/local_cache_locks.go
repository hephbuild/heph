package engine

import (
	"context"
	"errors"
	"github.com/hephbuild/heph/internal/hartifact"
	"github.com/hephbuild/heph/internal/hfs"
	"github.com/hephbuild/heph/internal/hlocks"
	"github.com/hephbuild/heph/internal/hslices"
	pluginv1 "github.com/hephbuild/heph/plugin/gen/heph/plugin/v1"
	"slices"
)

type CacheLocks struct {
	ms      []*hlocks.TMutex
	unlocks []func() error
}

func (c *CacheLocks) Add(m *hlocks.TMutex) {
	c.ms = append(c.ms, m)
}

func (c *CacheLocks) Lock(ctx context.Context) error {
	ctx, span := tracer.Start(ctx, "CacheLocks.Lock")
	defer span.End()

	return c.lock(ctx, false)
}

func (c *CacheLocks) RLock(ctx context.Context) error {
	ctx, span := tracer.Start(ctx, "CacheLocks.RLock")
	defer span.End()

	return c.lock(ctx, true)
}

func (c *CacheLocks) lock(ctx context.Context, ro bool) error {
	lock := (*hlocks.TMutex).Lock
	unlock := (*hlocks.TMutex).Unlock
	if ro {
		lock = (*hlocks.TMutex).RLock
		unlock = (*hlocks.TMutex).RUnlock
	}

	if len(c.unlocks) < len(c.ms) {
		c.unlocks = hslices.GrowLen(c.unlocks, len(c.ms)-len(c.unlocks))
	}

	for i, m := range c.ms {
		err := lock(m, ctx)
		if err != nil {
			err = errors.Join(err, c.Unlock())

			return err
		}

		c.unlocks[i] = func() error {
			return unlock(m)
		}
	}

	return nil
}

func (c *CacheLocks) Unlock() error {
	var errs error
	for i, l := range slices.Backward(c.unlocks) {
		err := l()
		if err != nil {
			errs = errors.Join(errs, err)
		}
		c.unlocks[i] = func() error { return nil }
	}

	return errs
}

func (c *CacheLocks) Lock2RLock(ctx context.Context) error {
	ctx, span := tracer.Start(ctx, "CacheLocks.Lock2RLock")
	defer span.End()

	var errs error
	for i, m := range c.ms {
		err := m.Lock2RLock(ctx)
		if err != nil {
			err = errors.Join(err, c.Unlock())

			return err
		}
		c.unlocks[i] = m.RUnlock
	}

	return errs
}

func (c *CacheLocks) Clone() *CacheLocks {
	return &CacheLocks{
		ms: slices.Clone(c.ms),
	}
}

func (e *Engine) lockCache(ctx context.Context, ref *pluginv1.TargetRef, outputs []string, hashin string, ro bool) (*CacheLocks, error) {
	ctx, span := tracer.Start(ctx, "lockCache")
	defer span.End()

	dirfs := hfs.At(e.Cache, ref.GetPackage(), e.targetDirName(ref), hashin)

	locks := &CacheLocks{}

	{
		outer := hlocks.NewFlock2(dirfs, "", hartifact.ManifestName+".outer.lock", true)
		inner := hlocks.NewFlock2(dirfs, "", hartifact.ManifestName+".inner.lock", true)

		locks.Add(hlocks.NewT(outer, inner))
	}

	for _, output := range outputs {
		outer := hlocks.NewFlock2(dirfs, "", "out_"+output+".outer.lock", true)
		inner := hlocks.NewFlock2(dirfs, "", "out_"+output+".inner.lock", true)

		locks.Add(hlocks.NewT(outer, inner))
	}

	if ro {
		err := locks.RLock(ctx)
		if err != nil {
			return nil, err
		}
	} else {
		err := locks.Lock(ctx)
		if err != nil {
			return nil, err
		}
	}

	return locks, nil
}
