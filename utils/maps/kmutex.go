package maps

import "sync"

type KMutex struct {
	o sync.Once
	m *Map[string, *sync.Mutex] // TODO: LRU
}

func (m *KMutex) Get(key string) *sync.Mutex {
	m.o.Do(func() {
		m.m = &Map[string, *sync.Mutex]{Default: func(string) *sync.Mutex {
			return &sync.Mutex{}
		}}
	})

	return m.m.Get(key)
}
