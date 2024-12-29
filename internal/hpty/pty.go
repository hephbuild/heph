package hpty

import (
	"context"
	"fmt"
	"io"
	"os"
	"os/signal"
	"syscall"

	"github.com/charmbracelet/x/term"
	ptylib "github.com/creack/pty"
	"github.com/hephbuild/hephv2/internal/hcore/hlog"
)

func WinSizeChan(ctx context.Context, f *os.File) (chan *ptylib.Winsize, func()) {
	szch := make(chan *ptylib.Winsize)
	ch := make(chan os.Signal, 1)
	signal.Notify(ch, syscall.SIGWINCH)

	go func() {
		for range ch {
			sz, err := ptylib.GetsizeFull(f)
			if err != nil {
				hlog.From(ctx).Error(fmt.Sprintf("error getting pty size: %s", err))
				return
			}

			szch <- sz
		}
	}()
	ch <- syscall.SIGWINCH // Initial resize.

	return szch, func() {
		// Cleanup signals when done.
		signal.Stop(ch)
		close(ch)
	}
}

func Create(ctx context.Context, r io.Reader, w io.Writer, szch <-chan *ptylib.Winsize) (term.File, error) {
	pty, tty, err := ptylib.Open()
	if err != nil {
		return nil, err
	}

	if szch != nil {
		// Handle pty size.
		go func() {
			for sz := range szch {
				if err := ptylib.Setsize(pty, sz); err != nil {
					hlog.From(ctx).Error(fmt.Sprintf("error resizing pty: %s", err))
				}
			}
		}()
	}

	go func() {
		_, _ = io.Copy(w, pty)
	}()
	if r != nil {
		go func() {
			_, _ = io.Copy(pty, r)
		}()
	}

	return tty, nil
}
