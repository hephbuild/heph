package xio

import (
	"context"
	"io"
)

func ContextReader[R io.Closer](ctx context.Context, r R) (R, func()) {
	ch := make(chan struct{})

	go func() {
		select {
		case <-ch:
		case <-ctx.Done():
			_ = r.Close()
		}
	}()

	return r, func() {
		close(ch)
	}
}
