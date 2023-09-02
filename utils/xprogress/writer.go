package xprogress

import (
	"io"
)

// WriterTap counts the bytes read through it.
type WriterTap struct {
	w io.Writer
	u *Counter
}

// NewWriterTap makes a new WriterTap that counts the bytes
// read through it.
func NewWriterTap(r io.Writer, u *Counter) *WriterTap {
	return &WriterTap{
		w: r,
		u: u,
	}
}

func (w *WriterTap) Write(p []byte) (n int, err error) {
	n, err = w.w.Write(p)
	w.u.AddN(int64(n))
	return
}
