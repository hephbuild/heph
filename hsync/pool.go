package hsync

import "sync"

type Pool[T any] struct {
	p    sync.Pool
	New  func() T
	once sync.Once
}

func (p *Pool[T]) Get() T {
	p.once.Do(func() {
		p.p.New = func() any {
			return p.New()
		}
	})

	return p.p.Get().(T)
}

func (p *Pool[T]) Put(v T) {
	p.p.Put(v)
}
