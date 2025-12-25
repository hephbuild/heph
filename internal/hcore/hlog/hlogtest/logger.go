package hlogtest

import (
	"context"
	"log/slog"
	"slices"
	"strings"
	"testing"

	"github.com/hephbuild/heph/internal/hcore/hlog"
)

func NewLogger(t testing.TB) hlog.Logger {
	return hlog.NewLogger(console{t: t})
}

type console struct {
	t testing.TB

	attrs []slog.Attr
}

func (c console) Enabled(ctx context.Context, level slog.Level) bool {
	return true
}

func renderAttr(sb *strings.Builder, attr slog.Attr) {
	sb.WriteString(" ")
	sb.WriteString(attr.Key)
	sb.WriteString("=")
	sb.WriteString(attr.Value.String())
}

func (c console) Handle(ctx context.Context, record slog.Record) error {
	var sb strings.Builder
	for _, attr := range c.attrs {
		renderAttr(&sb, attr)
	}
	record.Attrs(func(attr slog.Attr) bool {
		renderAttr(&sb, attr)

		return true
	})

	c.t.Logf("%s %s%s", record.Level.String(), record.Message, sb.String())

	return nil
}

func (c console) WithAttrs(attrs []slog.Attr) slog.Handler {
	c.attrs = slices.Clone(c.attrs)
	c.attrs = append(c.attrs, attrs...)

	return c
}

func (c console) WithGroup(name string) slog.Handler {
	return c
}
