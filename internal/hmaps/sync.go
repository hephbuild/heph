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
	_, ok := m.SetOkV(k, v)

	return ok
}

func (m *Sync[K, V]) SetOkV(k K, v V) (V, bool) {
	m.mu.Lock()
	defer m.mu.Unlock()

	if m.m == nil {
		m.m = map[K]V{}
	}

	if v, ok := m.m[k]; ok {
		return v, false
	}

	m.m[k] = v

	return v, true
}

func (m *Sync[K, V]) Delete(k K) {
	m.mu.Lock()
	defer m.mu.Unlock()

	delete(m.m, k)
}

func (m *Sync[K, V]) Values() iter.Seq[V] {
	return maps.Values(m.m)
}

func (m *Sync[K, V]) Keys() iter.Seq[K] {
	return maps.Keys(m.m)
}

func (m *Sync[K, V]) Range() iter.Seq2[K, V] {
	return maps.All(m.m)
}
