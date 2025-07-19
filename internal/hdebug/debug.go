//go:build debugger

package hdebug

import (
	"context"
	"net/http"
	"runtime/pprof"
)

func SetLabels(ctx context.Context, l func() []string) (context.Context, func()) {
	nctx := pprof.WithLabels(ctx, pprof.Labels(l()...))
	pprof.SetGoroutineLabels(nctx)

	return nctx, func() {
		pprof.SetGoroutineLabels(ctx)
	}
}

func Middleware(h http.Handler, l func() []string) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		ctx, cleanLabels := SetLabels(r.Context(), l)
		defer cleanLabels()

		h.ServeHTTP(w, r.WithContext(ctx))
	})
}
