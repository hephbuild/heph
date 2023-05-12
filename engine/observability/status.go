package observability

import (
	"context"
	"github.com/charmbracelet/lipgloss"
	"github.com/hephbuild/heph/log/log"
)

type StatusHandler interface {
	Status(status StatusFactory)
}

type StatusFactory interface {
	String(r *lipgloss.Renderer) string
}

type ctxkey struct{}

func ContextWithStatus(ctx context.Context, handler StatusHandler) context.Context {
	return context.WithValue(ctx, ctxkey{}, handler)
}

func Status(ctx context.Context, s StatusFactory) {
	if h, ok := ctx.Value(ctxkey{}).(StatusHandler); ok {
		h.Status(s)
	} else {
		r := log.Renderer()
		log.Default().Info(s.String(r))
	}
}

func StringStatus(status string) StatusFactory {
	return stringStatus(status)
}

type stringStatus string

func (s stringStatus) String(*lipgloss.Renderer) string {
	return string(s)
}
