package utils

import "sync"

type Once[T any] struct {
	o   sync.Once
	v   T
	err error
}

func (o *Once[T]) Do(f func() (T, error)) (T, error) {
	o.o.Do(func() {
		o.v, o.err = f()
	})

	return o.v, o.err
}

func (o *Once[T]) MustDo(f func() (T, error)) T {
	v, err := o.Do(f)
	if err != nil {
		panic(err)
	}

	return v
}
