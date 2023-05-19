package maps

import (
	"golang.org/x/exp/constraints"
	"sort"
	"sync"
)

type OMap[K constraints.Ordered, V any] struct {
	Map[K, V]
}

func (m *OMap[K, V]) Keys() []K {
	m.init()

	m.mu.RLock()
	defer m.mu.RUnlock()

	ks := make([]K, 0, len(m.m))

	for k := range m.m {
		ks = append(ks, k)
	}

	sort.Slice(ks, func(i, j int) bool {
		return ks[i] < ks[j]
	})

	return ks
}

type Map[K comparable, V any] struct {
	Default func(k K) V

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
	m.mu.Lock()
	defer m.mu.Unlock()

	delete(m.m, k)
}

func (m *Map[K, V]) GetOk(k K) (V, bool) {
	return m.getFast(k)
}

func (m *Map[K, V]) getFast(k K) (V, bool) {
	m.mu.RLock()
	defer m.mu.RUnlock()

	v, ok := m.m[k]
	return v, ok
}

func (m *Map[K, V]) Get(k K) V {
	return m.GetDefault(k, m.Default)
}

func (m *Map[K, V]) GetDefault(k K, def func(K) V) V {
	v, ok := m.getFast(k)
	if def == nil || ok {
		return v
	}

	m.init()

	m.mu.Lock()
	defer m.mu.Unlock()

	v, ok = m.m[k]
	if !ok {
		v = def(k)
		m.m[k] = v
	}

	return v
}

func (m *Map[K, V]) Has(k K) bool {
	_, ok := m.getFast(k)

	return ok
}

func (m *Map[K, V]) Raw() map[K]V {
	return m.m
}
