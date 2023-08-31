package xio

import (
	"io"
)

func Copy(dst io.Writer, src io.Reader, f func(written int64)) (int64, error) {
	t := &Tracker{
		OnWrite: func(written int64) {
			f(written)
		},
	}

	written, err := io.Copy(io.MultiWriter(dst, t), src)

	f(written)

	return written, err
}
