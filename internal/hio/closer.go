package hio

import (
	"io"
	"sync"
)

type readCloser struct {
	io.Reader
	io.Closer
}

func NewReadCloser(r io.Reader, c io.Closer) io.ReadCloser {
	return readCloser{
		Reader: r,
		Closer: c,
	}
}

type readCloserFunc struct {
	io.Reader
	CloserFunc func() error
}

func (r readCloserFunc) Close() error {
	if r.CloserFunc == nil {
		return nil
	}

	return r.CloserFunc()
}

func NewReadCloserFunc(r io.Reader, f func() error) io.ReadCloser {
	if f != nil {
		f = sync.OnceValue(f)
	}

	return readCloserFunc{
		Reader:     r,
		CloserFunc: f,
	}
}
