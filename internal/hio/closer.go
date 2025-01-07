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
