package status

import (
	"context"
	"github.com/charmbracelet/lipgloss"
	"github.com/hephbuild/heph/log/log"
)

type Handler interface {
	Status(status Statuser)
}

type Statuser interface {
	String(r *lipgloss.Renderer) string
}

type ctxkey struct{}

func ContextWithStatus(ctx context.Context, handler Handler) context.Context {
	return context.WithValue(ctx, ctxkey{}, handler)
}

func Emit(ctx context.Context, s Statuser) {
	if h, ok := ctx.Value(ctxkey{}).(Handler); ok {
		h.Status(s)
	} else {
		r := log.Renderer()
		log.Default().Info(s.String(r))
	}
}

func String(status string) Statuser {
	return stringStatus(status)
}

type stringStatus string

func (s stringStatus) String(*lipgloss.Renderer) string {
	return string(s)
}
