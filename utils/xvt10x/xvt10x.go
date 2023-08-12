package xvt10x

import (
	"github.com/charmbracelet/lipgloss"
	"github.com/creack/pty"
	"github.com/hinshun/vt10x"
	"io"
	"os"
	"os/exec"
	"strings"
	"syscall"
)

type Terminal struct {
	pty, tty *os.File
	onResize func()
	t        vt10x.Terminal
	sb       strings.Builder
}

func (t Terminal) Render(renderer *lipgloss.Renderer) string {
	t.t.Lock()
	defer t.t.Unlock()

	cols, rows := t.t.Size()

	sb := &t.sb
	sb.Reset()

	for y := 0; y < rows; y++ {
		for x := 0; x < cols; x++ {
			attr := t.t.Cell(x, y)

			sb.WriteString(renderGlyph(renderer, attr))
		}
		if y < rows-1 {
			sb.WriteString("\n")
		}
	}

	return sb.String()
}

func (t Terminal) Resize(cols, rows uint16) {
	if t.pty == nil || t.t == nil {
		return
	}

	if err := pty.Setsize(t.pty, &pty.Winsize{
		Cols: cols,
		Rows: rows,
	}); err != nil {
		return
	}
	t.onResize()
	t.t.Resize(int(cols), int(rows))
}

func (t Terminal) Close() error {
	if t.pty != nil {
		err := t.pty.Close()
		if err != nil {
			return err
		}
	}
	return nil
}

func (t Terminal) Size() (cols int, rows int) {
	if t.t == nil {
		return 0, 0
	}

	return t.t.Size()
}

func NewCmd(cmd *exec.Cmd) (Terminal, error) {
	return New(
		func(tty *os.File) {
			cmd.Stdin = tty
			cmd.Stdout = tty
			cmd.Stderr = tty
		}, func() {
			if cmd.Process != nil {
				_ = cmd.Process.Signal(syscall.SIGWINCH)
			}
		},
	)
}

func New(onSetup func(tty *os.File), onResize func()) (Terminal, error) {
	ptmx, tty, err := pty.Open()
	if err != nil {
		return Terminal{}, err
	}

	screen := vt10x.New(
		vt10x.WithWriter(tty),
	)

	icols, irows := screen.Size()
	if err := pty.Setsize(ptmx, &pty.Winsize{
		Cols: uint16(icols),
		Rows: uint16(irows),
	}); err != nil {
		_ = ptmx.Close()
		return Terminal{}, err
	}

	onSetup(tty)

	go func() {
		_, _ = io.Copy(screen, ptmx)
	}()

	return Terminal{
		pty:      ptmx,
		tty:      tty,
		onResize: onResize,
		t:        screen,
	}, nil
}
