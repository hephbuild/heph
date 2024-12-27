package hlog

import (
	"context"
	"io"
	"log/slog"
)

type loggerCtxKey struct{}

var nop = NewLogger(slog.NewTextHandler(io.Discard, &slog.HandlerOptions{}))

func From(ctx context.Context) Logger {
	l, ok := ctx.Value(loggerCtxKey{}).(Logger)
	if !ok {
		return nop
	}
	return l
}

func ContextWithLogger(ctx context.Context, l Logger) context.Context {
	return context.WithValue(ctx, loggerCtxKey{}, l)
}
