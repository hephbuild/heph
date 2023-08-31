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

type handlerKey struct{}

func ContextWithHandler(ctx context.Context, handler Handler) context.Context {
	return context.WithValue(ctx, handlerKey{}, handler)
}

var lastLogEmitted string

func Emit(ctx context.Context, s Statuser) {
	if h, ok := ctx.Value(handlerKey{}).(Handler); ok {
		h.Status(s)
	} else {
		r := log.Renderer()

		str := s.String(r)

		if str != lastLogEmitted {
			log.Default().Info(str)
			lastLogEmitted = str
		}
	}
}

func IsInteractive(ctx context.Context) bool {
	if _, ok := ctx.Value(handlerKey{}).(Handler); ok {
		return true
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
