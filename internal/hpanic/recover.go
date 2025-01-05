package hpanic

import (
	"fmt"
	"runtime"
)

type Error struct {
	Err   error
	Stack string
}

func (e Error) Error() string {
	return fmt.Sprintf("%v\n%s", e.Err, e.Stack)
}

func (e Error) Unwrap() error { return e.Err }

type Option interface {
	do(*options)
}

type optionFunc func(*options)

func (f optionFunc) do(o *options) {
	f(o)
}

type options struct {
	wrap func(err any) error
}

func RecoverV[T any](f func() (T, error), opts ...Option) (_ T, err error) {
	o := options{}
	for _, opt := range opts {
		opt.do(&o)
	}

	defer func() {
		if rerr := recover(); rerr != nil { //nolint:nestif
			if o.wrap != nil {
				err = o.wrap(rerr)
			} else {
				if rerrr, ok := rerr.(error); ok {
					err = rerrr
				} else {
					err = fmt.Errorf("%v", rerr)
				}
			}

			buf := make([]byte, 512)
			runtime.Stack(buf, false)
			err = Error{
				Err:   err,
				Stack: string(buf),
			}
		}
	}()

	return f()
}

func Recover(f func() error, opts ...Option) error {
	_, err := RecoverV[struct{}](func() (struct{}, error) {
		return struct{}{}, f()
	}, opts...)

	return err
}

func Wrap(f func(err any) error) Option {
	return optionFunc(func(o *options) {
		o.wrap = f
	})
}
