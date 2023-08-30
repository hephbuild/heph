package xio

import (
	"go.uber.org/multierr"
	"io"
)

func MultiCloser(closers ...func() error) io.Closer {
	return multiCloser{cs: closers}
}

type multiCloser struct {
	cs []func() error
}

func (r multiCloser) Close() error {
	var err error

	for _, c := range r.cs {
		cerr := c()
		if cerr != nil {
			err = multierr.Append(err, cerr)
		}
	}

	return err
}

type readCloser struct {
	io.Reader
	c io.Closer
}

func (r readCloser) Close() error {
	return r.c.Close()
}

func ReadCloser(r io.Reader, c io.Closer) io.ReadCloser {
	return readCloser{
		Reader: r,
		c:      c,
	}
}
