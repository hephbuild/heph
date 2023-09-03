package status

import (
	"context"
	"github.com/charmbracelet/lipgloss"
	"github.com/hephbuild/heph/log/log"
)

type Handler interface {
	Status(status Statuser)
	Interactive() bool
}

type Statuser interface {
	String(r *lipgloss.Renderer) string
}

type handlerKey struct{}

func ContextWithHandler(ctx context.Context, handler Handler) context.Context {
	return context.WithValue(ctx, handlerKey{}, handler)
}

func HandlerFromContext(ctx context.Context) (Handler, bool) {
	h, ok := ctx.Value(handlerKey{}).(Handler)
	return h, ok
}

type defaultHandler struct {
	lastEmitted string
}

func (h *defaultHandler) Interactive() bool {
	return false
}

func (h *defaultHandler) Status(s Statuser) {
	r := log.Renderer()

	str := s.String(r)

	if str != h.lastEmitted {
		log.Default().Info(str)
		h.lastEmitted = str
	}
}

var DefaultHandler Handler = &defaultHandler{}

func Emit(ctx context.Context, s Statuser) {
	h, ok := HandlerFromContext(ctx)
	if !ok {
		h = DefaultHandler
	}
	h.Status(s)
}

func EmitInteractive(ctx context.Context, s Statuser) {
	h, ok := HandlerFromContext(ctx)
	if !ok {
		h = DefaultHandler
	}
	if !h.Interactive() {
		return
	}
	h.Status(s)
}

func IsInteractive(ctx context.Context) bool {
	if h, ok := ctx.Value(handlerKey{}).(Handler); ok {
		return h.Interactive()
	}

	return false
}

func String(status string) Statuser {
	return stringStatus(status)
}

type stringStatus string

func (s stringStatus) String(*lipgloss.Renderer) string {
	return string(s)
}
