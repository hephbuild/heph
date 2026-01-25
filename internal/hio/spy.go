package hio

import "io"

type SpyLast struct {
	io.Writer

	LastByte byte
	Written  bool
}

func (t *SpyLast) Reset() {
	t.LastByte = 0
	t.Written = false
}

func (t *SpyLast) Write(p []byte) (n int, err error) { //nolint:nonamedreturns
	n, err = t.Writer.Write(p)
	if n > 0 {
		// Capture the last byte effectively written
		t.LastByte = p[n-1]
		t.Written = true
	}
	return
}
