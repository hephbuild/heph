package flock

import (
	"context"
	"fmt"
	log "heph/hlog"
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

func (m *mutex) Lock(ctx context.Context) error {
	select {
	case m.ch <- struct{}{}:
		return nil
	default:
		// continue
	}

	log.Warnf("Looks like another process has already acquired the lock for %s. Waiting for it to finish...", m.name)
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
