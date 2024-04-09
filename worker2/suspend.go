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

func Wait(ctx context.Context, f func()) {
	_ = WaitE(ctx, func() error {
		f()
		return nil
	})
}

func WaitE(ctx context.Context, f func() error) error {
	e := executionFromContext(ctx)
	if e == nil {
		err := f()
		if err != nil {
			return err
		}
	} else {
		e.Suspend()
		err := f()
		ack := e.Resume()
		<-ack
		if err != nil {
			return err
		}
	}
	return nil
}

func WaitDep(ctx context.Context, dep Dep) error {
	return WaitE(ctx, func() error {
		select {
		case <-dep.Wait():
			return nil
		case <-ctx.Done():
			return ctx.Err()
		}
	})
}

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

func WaitChanE[T error](ctx context.Context, ch <-chan T) error {
	return WaitE(ctx, func() error {
		select {
		case err := <-ch:
			return err
		case <-ctx.Done():
			return ctx.Err()
		}
	})
}
