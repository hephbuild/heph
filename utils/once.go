package utils

import "sync"

type Once[T any] struct {
	RetryOnError bool
	m            sync.Mutex
	ran          bool
	v            T
	err          error
}

func (o *Once[T]) Do(f func() (T, error)) (T, error) {
	if o.ran {
		return o.v, o.err
	}

	o.m.Lock()
	defer o.m.Unlock()

	if o.ran {
		return o.v, o.err
	}

	v, err := f()
	if err != nil && o.RetryOnError {
		// noop
	} else {
		o.v, o.err, o.ran = v, err, true
	}

	return v, err
}

func (o *Once[T]) MustDo(f func() (T, error)) T {
	v, err := o.Do(f)
	if err != nil {
		panic(err)
	}

	return v
}
