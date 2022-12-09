package flock

import (
	log "github.com/sirupsen/logrus"
	"sync"
)

func NewMutex(name string) Locker {
	return &mutex{name: name}
}

type mutex struct {
	name string
	sync.Mutex
}

func (m *mutex) Unlock() error {
	m.Mutex.Unlock()
	return nil
}

func (m *mutex) Lock() error {
	if m.Mutex.TryLock() {
		return nil
	}

	log.Warnf("Looks like another process has already acquired the lock for %s. Waiting for it to finish...", m.name)
	m.Mutex.Lock()

	return nil
}

func (*mutex) Clean() error {
	return nil
}
