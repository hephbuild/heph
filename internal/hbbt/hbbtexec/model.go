package hbbtexec

import (
	"errors"
	"fmt"
	"io"
	"sync"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/x/term"
	"github.com/hephbuild/heph/internal/hbbt/hbbtlog"
)

type ExecFunc[T any] func(args RunArgs) (T, error)

type execCmdFunc func(args RunArgs) error

type execCmd struct {
	f              execCmdFunc
	stdin          io.Reader
	stdout, stderr io.Writer

	restore func()
}

func (e *execCmd) makeRaw() error {
	if f, ok := e.stdin.(term.File); ok && term.IsTerminal(f.Fd()) {
		previousState, err := term.MakeRaw(f.Fd())
		if err != nil {
			return fmt.Errorf("error entering raw mode: %w", err)
		}
		e.restore = func() {
			err := term.Restore(f.Fd(), previousState)
			if err != nil { //nolint:staticcheck
				// fmt.Println("RESTORE", err)
				// TODO: log
			}
		}
	}

	return nil
}

type RunArgs struct {
	Stdin          io.Reader
	Stdout, Stderr io.Writer
	MakeRaw        func() error
}

func (e *execCmd) Run() error {
	defer func() {
		if e.restore != nil {
			e.restore()
			e.restore = nil
		}
	}()

	return e.f(RunArgs{
		MakeRaw: sync.OnceValue(e.makeRaw),
		Stdin:   e.stdin,
		Stdout:  e.stdout,
		Stderr:  e.stderr,
	})
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
	// 3. pause the logs, since bbt will take control of the term again
	// defer e.w.Write([]byte("\n"))
	defer e.hijacker.SetMode(hbbtlog.LogHijackerModeWait)

	return e.c.Run()
}

func (e *execCmdWrapper) SetStdin(r io.Reader) {
	// 2. resume the logs, but without hijacking since bbt doesnt control the term anymore
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

	cmd := m.Exec(NewExecFunc(func(args RunArgs) error {
		v, err := f(args)

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
		// 1. before starting, pause the logs
		m.hijacker.SetMode(hbbtlog.LogHijackerModeWait)

		cc := &execCmdWrapper{
			c:        c,
			hijacker: m.hijacker,
		}

		return tea.Exec(cc, func(err error) tea.Msg {
			// 4. resume the logs, but since bbt controls the term, send them to bbt
			m.hijacker.SetMode(hbbtlog.LogHijackerModeHijack)

			var cmd tea.Msg
			if fn != nil {
				cmd = fn(err)
			}

			return cmd
		})()
	}
}

// New creates an exec controller, hijacker must be set in context:
//
//	ctx = hlog.NewContextWithHijacker(ctx, hijacker.Handler)
func New(hijacker hbbtlog.Hijacker) Model {
	m := Model{
		hijacker: hijacker,
	}

	return m
}
