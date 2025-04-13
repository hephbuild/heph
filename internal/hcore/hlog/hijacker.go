package hlog

import (
	"context"
	"log/slog"
	"slices"
)

type HandleFunc = func(ctx context.Context, record slog.Record) error
type HijackFunc = func(next HandleFunc, ctx context.Context, attrs []slog.Attr, record slog.Record) error

type hijackHandler struct {
	h     slog.Handler
	f     HijackFunc
	attrs []slog.Attr
}

func (h hijackHandler) Enabled(ctx context.Context, level slog.Level) bool {
	return h.h.Enabled(ctx, level)
}

func (h hijackHandler) Handle(ctx context.Context, record slog.Record) error {
	if f := h.f; f != nil {
		return f(h.h.Handle, ctx, h.attrs, record)
	}

	return h.h.Handle(ctx, record)
}

func (h hijackHandler) WithAttrs(attrs []slog.Attr) slog.Handler {
	h.h = h.h.WithAttrs(attrs)
	h.attrs = slices.Clone(h.attrs)
	h.attrs = append(h.attrs, attrs...)
	return h
}

func (h hijackHandler) WithGroup(name string) slog.Handler {
	h.h = h.h.WithGroup(name)
	return h
}

func NewContextWithHijacker(ctx context.Context, f HijackFunc) context.Context {
	l := From(ctx)
	h := l.Handler()

	return ContextWithLogger(ctx, slog.New(hijackHandler{h: h, f: f}))
}
