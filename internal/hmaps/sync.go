package hmaps

import (
	"iter"
	"maps"
	"sync"
)

type Sync[K comparable, V any] struct {
	m  map[K]V
	mu sync.RWMutex
}

func (m *Sync[K, V]) GetOk(k K) (V, bool) {
	m.mu.RLock()
	defer m.mu.RUnlock()

	v, ok := m.m[k]

	return v, ok
}

func (m *Sync[K, V]) Get(k K) V {
	v, _ := m.GetOk(k)

	return v
}

func (m *Sync[K, V]) Set(k K, v V) {
	m.mu.Lock()
	defer m.mu.Unlock()

	if m.m == nil {
		m.m = map[K]V{}
	}

	m.m[k] = v
}

func (m *Sync[K, V]) SetOk(k K, v V) bool {
	m.mu.Lock()
	defer m.mu.Unlock()

	if m.m == nil {
		m.m = map[K]V{}
	}

	_, ok := m.m[k]
	if ok {
		return false
	}

	m.m[k] = v

	return true
}

func (m *Sync[K, V]) Values() iter.Seq[V] {
	return maps.Values(m.m)
}
