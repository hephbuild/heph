package utils

import "sync"

type KMutex struct {
	mu sync.Mutex
	m  map[string]*sync.Mutex // TODO: LRU
}

func (m *KMutex) Get(key string) *sync.Mutex {
	m.mu.Lock()
	defer m.mu.Unlock()

	if mu, ok := m.m[key]; ok {
		return mu
	}

	if m.m == nil {
		m.m = map[string]*sync.Mutex{}
	}

	mu := &sync.Mutex{}
	m.m[key] = mu

	return mu
}
