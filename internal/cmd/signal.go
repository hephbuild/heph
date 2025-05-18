package cmd

import (
	"context"
	"errors"
	"github.com/hephbuild/heph/internal/hsoftcontext"
	"os"
	"os/signal"
)

func newSignalNotifyContext(ctx context.Context) (context.Context, context.CancelFunc) {
	ctx, cancel := hsoftcontext.WithCancel(ctx)

	ch := make(chan os.Signal, 1)
	signal.Notify(ch, os.Interrupt)

	go func() {
		for range ch {
			cancel(errors.New("ctrl+c"))
		}
	}()

	return ctx, func() {
		cancel(nil)
		signal.Stop(ch)
	}
}
