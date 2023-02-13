package maps

import (
	"golang.org/x/exp/constraints"
	"sort"
	"sync"
)

type Map[K constraints.Ordered, V any] struct {
	Default func() V

	mu sync.RWMutex
	m  map[K]V
	o  sync.Once
}

func (m *Map[K, V]) init() {
	m.o.Do(func() {
		if m.m == nil {
			m.m = map[K]V{}
		}
	})
}

func (m *Map[K, V]) Set(k K, v V) {
	m.init()

	m.mu.Lock()
	defer m.mu.Unlock()

	m.m[k] = v
}

func (m *Map[K, V]) Delete(k K) {
	m.init()

	m.mu.Lock()
	defer m.mu.Unlock()

	delete(m.m, k)
}

func (m *Map[K, V]) GetOk(k K) (V, bool) {
	m.init()

	m.mu.RLock()
	defer m.mu.RUnlock()

	v, ok := m.m[k]

	return v, ok
}

func (m *Map[K, V]) Get(k K) V {
	m.init()

	if m.Default == nil {
		m.mu.RLock()
		defer m.mu.RUnlock()
	} else {
		m.mu.Lock()
		defer m.mu.Unlock()
	}

	v, ok := m.m[k]
	if !ok && m.Default != nil {
		v = m.Default()
		m.m[k] = v
	}

	return v
}

func (m *Map[K, V]) Has(k K) bool {
	m.init()

	m.mu.RLock()
	defer m.mu.RUnlock()

	_, ok := m.m[k]

	return ok
}

func (m *Map[K, V]) Keys() []K {
	m.init()

	m.mu.RLock()
	defer m.mu.RUnlock()

	ks := make([]K, len(m.m))

	for k := range m.m {
		ks = append(ks, k)
	}

	sort.Slice(ks, func(i, j int) bool {
		return ks[i] < ks[j]
	})

	return ks
}

func (m *Map[K, V]) Map() map[K]V {
	return m.m
}
