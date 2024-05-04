package worker2

import "context"

var executionKey struct{}

func contextWithExecution(ctx context.Context, e *Execution) context.Context {
	return context.WithValue(ctx, executionKey, e)
}

func executionFromContext(ctx context.Context) *Execution {
	e, _ := ctx.Value(executionKey).(*Execution)
	return e
}

// Wait suspends execution until the function returns
func Wait(ctx context.Context, f func()) {
	_ = WaitE(ctx, func() error {
		f()
		return nil
	})
}

// WaitE suspends execution until the function returns
func WaitE(ctx context.Context, f func() error) error {
	e := executionFromContext(ctx)
	if e == nil {
		err := f()
		if err != nil {
			return err
		}
	} else {
		sb := e.Suspend()
		err := f()
		ack := sb.Resume()
		<-ack
		if err != nil {
			return err
		}
	}
	return nil
}

// WaitDep suspends execution until Dep completes or the context gets canceled
func WaitDep(ctx context.Context, dep Dep) error {
	return WaitChan(ctx, dep.Wait())
}

// WaitChan suspends execution until reading from the channel returns or the context gets canceled
func WaitChan[T any](ctx context.Context, ch <-chan T) error {
	return WaitE(ctx, func() error {
		select {
		case <-ch:
			return nil
		case <-ctx.Done():
			return ctx.Err()
		}
	})
}

// WaitChanReceive suspends execution until reading from the channel returns or the context gets canceled
// Unlike WaitChan, it will return the value
func WaitChanReceive[T any](ctx context.Context, ch <-chan T) (T, error) {
	var out T
	err := WaitE(ctx, func() error {
		select {
		case v := <-ch:
			out = v
			return nil
		case <-ctx.Done():
			return ctx.Err()
		}
	})
	return out, err
}

// WaitChanSend suspends execution until sending to the channel succeeds or the context gets canceled
func WaitChanSend[T any](ctx context.Context, ch chan<- T, v T) error {
	return WaitE(ctx, func() error {
		select {
		case ch <- v:
			return nil
		case <-ctx.Done():
			return ctx.Err()
		}
	})
}
