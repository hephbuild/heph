package locks

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/status"
)

func NewMutex(name string) Locker {
	return &mutex{name: name, ch: make(chan struct{}, 1)}
}

type mutex struct {
	name string
	ch   chan struct{}
}

func (m *mutex) Unlock() error {
	select {
	case <-m.ch:
		return nil
	default:
		return fmt.Errorf("unlock of unlocked mutex")
	}
}

func (m *mutex) TryLock(context.Context) (bool, error) {
	select {
	case m.ch <- struct{}{}:
		return true, nil
	default:
		return false, nil
	}
}

func (m *mutex) Lock(ctx context.Context) error {
	ok, err := m.TryLock(ctx)
	if err != nil {
		return err
	}
	if ok {
		return nil
	}

	status.Emit(ctx, status.String(fmt.Sprintf("Another process locked %v, waiting...", m.name)))

	select {
	case m.ch <- struct{}{}:
		return nil
	case <-ctx.Done():
		return fmt.Errorf("acquire lock for %v: %v", m.name, ctx.Err())
	}
}

func (*mutex) Clean() error {
	return nil
}
