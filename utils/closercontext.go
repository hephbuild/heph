package utils

import (
	"context"
	"io"
)

func CloserContext(c io.Closer, ctx context.Context) func() {
	doneCh := make(chan struct{})

	go func() {
		select {
		case <-ctx.Done():
			_ = c.Close()
		case <-doneCh:
			// discard goroutine
		}
	}()

	return func() {
		close(doneCh)
	}
}
