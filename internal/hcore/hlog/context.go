package hlog

import (
	"context"
	"io"
	"log/slog"
)

type loggerCtxKey struct{}

var nop = slog.New(slog.NewTextHandler(io.Discard, &slog.HandlerOptions{}))

func From(ctx context.Context) *slog.Logger {
	l, ok := ctx.Value(loggerCtxKey{}).(*slog.Logger)
	if !ok {
		return nop
	}
	return l
}

func ContextWithLogger(ctx context.Context, l *slog.Logger) context.Context {
	return context.WithValue(ctx, loggerCtxKey{}, l)
}
