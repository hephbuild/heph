package worker2

import "context"

var executionKey struct{}

func contextWithExecution(ctx context.Context, e *Execution) context.Context {
	return context.WithValue(ctx, executionKey, e)
}

func executionFromContext(ctx context.Context) *Execution {
	return ctx.Value(executionKey).(*Execution)
}

func Wait(ctx context.Context, f func()) {
	e := executionFromContext(ctx)
	if e == nil {
		f()
	} else {
		e.Suspend()
		f()
		ack := e.Resume()
		<-ack
	}
}
