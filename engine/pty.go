package engine

import (
	ptylib "github.com/creack/pty"
	"github.com/hephbuild/heph/log/log"
	"github.com/mattn/go-isatty"
	"golang.org/x/term"
	"io"
	"os"
	"os/signal"
	"syscall"
)

func multiWriterNil(ws ...io.Writer) io.Writer {
	var mws []io.Writer
	for _, w := range ws {
		if w == nil {
			continue
		}

		mws = append(mws, w)
	}

	if len(mws) == 0 {
		return nil
	}

	if len(mws) == 1 {
		return mws[0]
	}

	return io.MultiWriter(mws...)
}

func isWriterTerminal(w io.Writer) bool {
	if f, ok := w.(interface{ Fd() uintptr }); ok {
		return isatty.IsTerminal(f.Fd())
	}

	return false
}

func stdinWinSizeCh() (chan *ptylib.Winsize, func()) {
	szch := make(chan *ptylib.Winsize)
	ch := make(chan os.Signal, 1)
	signal.Notify(ch, syscall.SIGWINCH)

	go func() {
		for range ch {
			sz, err := ptylib.GetsizeFull(os.Stdin)
			if err != nil {
				log.Debugf("error getting pty size: %s", err)
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

func stdinRawMode(ptmx *os.File) (error, func()) {
	// Set stdin in raw mode.
	oldState, err := term.MakeRaw(int(os.Stdin.Fd()))
	if err != nil {
		return err, nil
	}

	// Copy stdin to the pty and the pty to stdout.
	// NOTE: The goroutine will keep reading until the next keystroke before returning.
	go func() { _, _ = io.Copy(ptmx, os.Stdin) }()

	return nil, func() {
		_ = term.Restore(int(os.Stdin.Fd()), oldState)
	}
}

func createPty(w io.Writer, szch <-chan *ptylib.Winsize) (io.Writer, error, func()) {
	ptmx, tty, err := ptylib.Open()
	if err != nil {
		return nil, err, nil
	}

	if szch != nil {
		// Handle pty size.
		go func() {
			for sz := range szch {
				if err := ptylib.Setsize(ptmx, sz); err != nil {
					log.Errorf("error resizing pty: %s", err)
				}
			}
		}()
	}

	go func() {
		_, _ = io.Copy(w, ptmx)
	}()

	return tty, nil, func() {
		_ = tty.Close()
	}
}
