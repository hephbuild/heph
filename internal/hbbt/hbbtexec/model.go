package hbbtexec

import (
	"context"
	"errors"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/hephbuild/hephv2/internal/hbbt/hbbtlog"
	"github.com/hephbuild/hephv2/internal/hcore/hlog"
	"io"
)

type ExecFunc[T any] func(stdin io.Reader, stdout, stderr io.Writer) (T, error)

type execCmdFunc func(stdin io.Reader, stdout, stderr io.Writer) error

type execCmd struct {
	f              execCmdFunc
	stdin          io.Reader
	stdout, stderr io.Writer
}

func (e *execCmd) Run() error {
	return e.f(e.stdin, e.stdout, e.stderr)
}

func (e *execCmd) SetStdin(r io.Reader) {
	e.stdin = r
}

func (e *execCmd) SetStdout(w io.Writer) {
	e.stdout = w
}

func (e *execCmd) SetStderr(w io.Writer) {
	e.stderr = w
}

func NewExecFunc(f execCmdFunc) tea.ExecCommand {
	return &execCmd{f: f}
}

type execCmdWrapper struct {
	c        tea.ExecCommand
	hijacker hbbtlog.Hijacker
	w        io.Writer
}

func (e *execCmdWrapper) Run() error {
	// 4. pause the logs, since bbt will take control of the term again
	//defer e.w.Write([]byte("\n"))
	defer e.hijacker.SetMode(hbbtlog.LogHijackerModeWait)

	return e.c.Run()
}

func (e *execCmdWrapper) SetStdin(r io.Reader) {
	// 3. resume the logs, but without hijacking since bbt doesnt control the term anymore
	e.hijacker.SetMode(hbbtlog.LogHijackerModeDisabled)

	e.c.SetStdin(r)
}

func (e *execCmdWrapper) SetStdout(w io.Writer) {
	e.c.SetStdout(w)
	e.w = w
}

func (e *execCmdWrapper) SetStderr(w io.Writer) {
	e.c.SetStderr(w)
}

type Model struct {
	hijacker hbbtlog.Hijacker
}

type runError struct {
	err error
}

func (r runError) Error() string {
	return r.err.Error()
}

func (r runError) Is(err error) bool {
	_, ok := err.(runError)

	return ok
}

func (r runError) Unwrap() error {
	return r.err
}

func Run[T any](m Model, send func(tea.Msg), f ExecFunc[T]) (T, error) {
	type container struct {
		v   T
		err error
	}

	resCh := make(chan container, 1)
	returnedCh := make(chan error, 1)

	cmd := m.Exec(NewExecFunc(func(stdin io.Reader, stdout, stderr io.Writer) error {
		v, err := f(stdin, stdout, stderr)

		resCh <- container{v: v, err: err}

		if err != nil {
			return runError{err: err}
		}

		return nil
	}), func(err error) tea.Msg {
		if errors.Is(err, runError{}) {
			returnedCh <- nil
		} else { // This should be a RestoreTerminal error
			returnedCh <- err
		}

		return nil
	})
	go send(cmd())

	res := <-resCh
	returnErr := <-returnedCh

	return res.v, errors.Join(res.err, returnErr)
}

func (m Model) Exec(c tea.ExecCommand, fn tea.ExecCallback) tea.Cmd {
	return func() tea.Msg {
		// 2. before starting, pause the logs
		m.hijacker.SetMode(hbbtlog.LogHijackerModeWait)

		cc := &execCmdWrapper{
			c:        c,
			hijacker: m.hijacker,
		}

		return tea.Exec(cc, func(err error) tea.Msg {
			// 5. resume the logs, but since bbt controls the term, send them to bbt
			m.hijacker.SetMode(hbbtlog.LogHijackerModeHijack)

			var cmd tea.Msg
			if fn != nil {
				cmd = fn(err)
			}

			return cmd
		})()
	}
}

func New(ctx context.Context, hijacker hbbtlog.Hijacker) (context.Context, Model) {
	// 1. set the hijacker in ctx
	ctx = hlog.NewContextWithHijacker(ctx, hijacker.Handler)

	m := Model{
		hijacker: hijacker,
	}

	return ctx, m
}
