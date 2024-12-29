package hlocks

import (
	"context"
	"fmt"

	"github.com/hephbuild/hephv2/internal/hcore/hlog"
	golock "github.com/viney-shih/go-lock"
)

func NewMutex(name string) RWLocker {
	return &mutex{name: name, m: golock.NewCASMutex()}
}

type mutex struct {
	name string
	m    *golock.CASMutex
}

func (m *mutex) TryLock(ctx context.Context) (bool, error) {
	return m.m.TryLock(), ctx.Err()
}

func (m *mutex) Lock(ctx context.Context) error {
	ok := m.m.TryLock()
	if ok {
		return nil
	}

	hlog.From(ctx).Debug(fmt.Sprintf("Another process locked %v, waiting...", m.name))

	m.m.TryLockWithContext(ctx)

	return ctx.Err()
}

func (m *mutex) Unlock() error {
	m.m.Unlock()

	return nil
}

func (m *mutex) TryRLock(ctx context.Context) (bool, error) {
	return m.m.RTryLock(), ctx.Err()
}

func (m *mutex) RLock(ctx context.Context) error {
	ok := m.m.RTryLock()
	if ok {
		return nil
	}

	hlog.From(ctx).Debug(fmt.Sprintf("Another process locked %v, waiting...", m.name))

	m.m.RTryLockWithContext(ctx)

	return ctx.Err()
}

func (m *mutex) RUnlock() error {
	m.m.RUnlock()

	return nil
}

func (*mutex) Clean() error {
	return nil
}
