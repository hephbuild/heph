package xio

import (
	"context"
	"io"
)

func ContextCloser[C io.Closer](ctx context.Context, c C) (C, func()) {
	ch := make(chan struct{})

	go func() {
		select {
		case <-ch:
		case <-ctx.Done():
			_ = c.Close()
		}
	}()

	return c, func() {
		close(ch)
	}
}
