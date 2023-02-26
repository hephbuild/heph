package maps

import (
	"github.com/puzpuzpuz/xsync/v2"
	"golang.org/x/exp/constraints"
	"sort"
	"sync"
)

type Map[K constraints.Ordered, V any] struct {
	Default func(k K) V

	mu *xsync.RBMutex
	m  map[K]V
	o  sync.Once
}

func (m *Map[K, V]) init() {
	m.o.Do(func() {
		if m.m == nil {
			m.m = map[K]V{}
		}
		if m.mu == nil {
			m.mu = xsync.NewRBMutex()
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
	return m.getFast(k)
}

func (m *Map[K, V]) getFast(k K) (V, bool) {
	m.init()

	tok := m.mu.RLock()
	defer m.mu.RUnlock(tok)

	v, ok := m.m[k]
	return v, ok
}

func (m *Map[K, V]) Get(k K) V {
	v, ok := m.getFast(k)
	if m.Default == nil || ok {
		return v
	}

	m.init()

	m.mu.Lock()
	defer m.mu.Unlock()

	v, ok = m.m[k]
	if !ok {
		v = m.Default(k)
		m.m[k] = v
	}

	return v
}

func (m *Map[K, V]) Has(k K) bool {
	_, ok := m.getFast(k)

	return ok
}

func (m *Map[K, V]) Keys() []K {
	m.init()

	tok := m.mu.RLock()
	defer m.mu.RUnlock(tok)

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
