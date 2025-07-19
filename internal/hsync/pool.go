package hsync

import "sync"

type Pool[T any] struct {
	p    sync.Pool
	New  func() T
	once sync.Once
}

func (p *Pool[T]) init() {
	p.p.New = func() any {
		return p.New()
	}
}

func (p *Pool[T]) Get() T {
	p.once.Do(p.init)

	return p.p.Get().(T) //nolint:errcheck
}

func (p *Pool[T]) Put(v T) {
	p.p.Put(v)
}
