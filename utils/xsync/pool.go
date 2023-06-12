package xsync

import "sync"

type Pool[T any] struct {
	New func() T
	p   sync.Pool
	o   sync.Once
}

func (p *Pool[T]) init() {
	p.p = sync.Pool{New: func() any {
		return p.New()
	}}
}

func (p *Pool[T]) Get() T {
	p.o.Do(p.init)
	return p.p.Get().(T)
}

func (p *Pool[T]) Put(v T) {
	p.o.Do(p.init)
	p.p.Put(v)
}
