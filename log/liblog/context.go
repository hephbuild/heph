package liblog

import (
	"context"
)

type key struct{}

func FromContext(ctx context.Context) Logger {
	if l, ok := ctx.Value(key{}).(Logger); ok {
		return l
	}

	return Default()
}

func ContextWith(ctx context.Context, l Logger) context.Context {
	return context.WithValue(ctx, key{}, l)
}
