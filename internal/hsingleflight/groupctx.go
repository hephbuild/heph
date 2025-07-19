package hsingleflight

import (
	"context"
	"errors"
)

type GroupCtx[K comparable, V any] struct {
	g Group[K, V]
}

type contextCanceledError struct {
	err error
}

func (e contextCanceledError) Error() string {
	return e.err.Error()
}

func (g *GroupCtx[K, V]) Do(ctx context.Context, key K, fn func(ctx context.Context) (V, error)) (V, error, bool) {
	return g.DoHandle(ctx, key, fn, nil)
}

func (g *GroupCtx[K, V]) DoHandle(ctx context.Context, key K, fn func(ctx context.Context) (V, error), onCompleted func(context.Context, V, error)) (V, error, bool) {
	for {
		v, err, shared := g.g.Do(key, func() (V, error) {
			v, err := fn(ctx)
			if err != nil && ctx.Err() != nil {
				// fn failed and ctx is canceled, fn probably failed because ctx is canceled
				g.g.Forget(key)

				return v, contextCanceledError{err: err}
			}

			if onCompleted != nil {
				onCompleted(ctx, v, err)
			}

			return v, err
		})
		if err != nil {
			var ccerr contextCanceledError
			if errors.As(err, &ccerr) {
				if ctx.Err() == nil {
					// Do failed because context got canceled and current ctx isnt canceled, retry
					continue
				}

				err = ccerr.err
			}
		}

		return v, err, shared
	}
}

func (g *GroupCtx[K, V]) Forget(key K) {
	g.g.Forget(key)
}
