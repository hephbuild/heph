package hbbtexec

import (
	"errors"
	"fmt"
	"io"
	"os"
	"sync"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/x/term"
	"github.com/hephbuild/heph/internal/hbbt/hbbtlog"
)

type ExecFunc func(args RunArgs) error

type execCmdFunc func(args RunArgs) error

type execCmd struct {
	f              execCmdFunc
	stdin          io.Reader
	stdout, stderr io.Writer

	restore func()
}

func MakeRaw(r io.Reader) (func(), error) {
	if f, ok := r.(term.File); ok && term.IsTerminal(f.Fd()) {
		previousState, err := term.MakeRaw(f.Fd())
		if err != nil {
			return nil, fmt.Errorf("error entering raw mode: %w", err)
		}
		return func() {
			err := term.Restore(f.Fd(), previousState)
			if err != nil { //nolint:staticcheck
				// fmt.Println("RESTORE", err)
				// TODO: log
			}
		}, nil
	}

	return func() {}, nil
}

func (e *execCmd) makeRaw() error {
	var err error
	e.restore, err = MakeRaw(e.stdin)

	return err
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
	// bbt has its output set to stderr, to prevent the CLI from outputting on stderr too,
	// we rely on os-provided stdout/stderr directly
	e.c.SetStdout(os.Stdout)
	e.w = w
}

func (e *execCmdWrapper) SetStderr(w io.Writer) {
	e.c.SetStderr(os.Stderr)
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

type StartMsg struct{}
type EndMsg struct{}

func Run(m Model, send func(tea.Msg), f ExecFunc) error {
	type container struct {
		err error
	}

	resCh := make(chan container, 1)
	errCh := make(chan error, 1)

	cmd := m.Exec(NewExecFunc(func(args RunArgs) error {
		err := f(args)

		resCh <- container{err: err}

		if err != nil {
			return runError{err: err}
		}

		return nil
	}), func(err error) tea.Msg {
		if errors.Is(err, runError{}) {
			errCh <- nil
		} else { // This should be a RestoreTerminal error
			errCh <- err
		}

		return nil
	})
	go send(cmd())

	res := <-resCh
	returnErr := <-errCh

	return errors.Join(res.err, returnErr)
}

func (m Model) Exec(c tea.ExecCommand, fn tea.ExecCallback) tea.Cmd {
	return func() tea.Msg {
		// 1. before starting, pause the logs
		m.hijacker.SetMode(hbbtlog.LogHijackerModeWait)

		cc := &execCmdWrapper{
			c:        c,
			hijacker: m.hijacker,
		}

		return tea.Sequence(
			func() tea.Msg {
				return StartMsg{}
			},
			tea.Exec(cc, func(err error) tea.Msg {
				// 4. resume the logs, but since bbt controls the term, send them to bbt
				m.hijacker.SetMode(hbbtlog.LogHijackerModeHijack)

				var cmd tea.Cmd
				if fn != nil {
					msg := fn(err)
					if msg != nil {
						cmd = func() tea.Msg {
							return msg
						}
					}
				}

				return tea.Sequence(
					func() tea.Msg {
						return EndMsg{}
					},
					cmd,
				)()
			}),
		)()
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
