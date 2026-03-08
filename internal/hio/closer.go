package hio

import "io"

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
	return r.CloserFunc()
}

func NewReadCloserFunc(r io.Reader, f func() error) io.ReadCloser {
	return readCloserFunc{
		Reader:     r,
		CloserFunc: f,
	}
}
