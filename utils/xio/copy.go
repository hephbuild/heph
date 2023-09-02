package xio

import (
	"io"
)

func Copy(dst io.Writer, src io.Reader, f func(written int64)) (int64, error) {
	return CopyBuffer(dst, src, nil, f)
}

func CopyBuffer(dst io.Writer, src io.Reader, buf []byte, f func(written int64)) (int64, error) {
	t := &Tracker{
		OnWrite: func(written int64) {
			f(written)
		},
	}

	w := io.MultiWriter(dst, t)

	written, err := io.CopyBuffer(w, src, buf)

	if written == 0 {
		_, err := w.Write([]byte{})
		if err != nil {
			return written, err
		}
	}

	f(written)

	return written, err
}
