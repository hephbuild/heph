package hlogtest

import (
	"context"
	"log/slog"
	"testing"

	"github.com/hephbuild/hephv2/internal/hcore/hlog"
)

func NewLogger(t testing.TB) hlog.Logger {
	return hlog.NewLogger(console{t: t})
}

type console struct {
	t testing.TB
}

func (c console) Enabled(ctx context.Context, level slog.Level) bool {
	return true
}

func (c console) Handle(ctx context.Context, record slog.Record) error {
	c.t.Logf("%s", record.Message)

	return nil
}

func (c console) WithAttrs(attrs []slog.Attr) slog.Handler {
	return c
}

func (c console) WithGroup(name string) slog.Handler {
	return c
}
