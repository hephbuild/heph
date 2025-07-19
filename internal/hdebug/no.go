//go:build !debugger

package hdebug

import (
	"context"
	"net/http"
)

func noop() {}

func SetLabels(ctx context.Context, l func() []string) (context.Context, func()) {
	return ctx, noop
}

func Middleware(h http.Handler, l func() []string) http.Handler {
	return h
}
