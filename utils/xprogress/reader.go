package xprogress

import (
	"io"
)

// ReaderTap counts the bytes read through it.
type ReaderTap struct {
	r io.Reader
	u *Unit
}

// NewReaderTap makes a new ReaderTap that counts the bytes
// read through it.
func NewReaderTap(r io.Reader, u *Unit) *ReaderTap {
	return &ReaderTap{
		r: r,
		u: u,
	}
}

func (r *ReaderTap) Read(p []byte) (n int, err error) {
	n, err = r.r.Read(p)
	r.u.AddN(int64(n))
	return
}
