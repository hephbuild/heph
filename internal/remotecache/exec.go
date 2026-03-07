package remotecache

import (
	"context"
	"errors"
	"io"
	"os/exec"
	"strings"
	"sync"

	"github.com/hephbuild/heph/lib/pluginsdk"
)

const DriverNameExec = "exec"

func NewExec(args []string) (*Exec, error) {
	return &Exec{args: args}, nil
}

const DriverNameSh = "sh"

func NewSh(cmd string) (*Exec, error) {
	return NewExec([]string{"sh", "-u", "-e", "-c", cmd})
}

var _ pluginsdk.Cache = (*Exec)(nil)

type Exec struct {
	args              []string
	notFoundSentinels []string
}

func (e Exec) Store(ctx context.Context, key string, r io.Reader) error {
	cmd := exec.CommandContext(ctx, e.args[0], e.args[1:]...) //nolint:gosec
	cmd.Env = append(cmd.Environ(), "CACHE_KEY="+key)
	cmd.Stdin = r

	return cmd.Run()
}

type execReader struct {
	cmd               *exec.Cmd
	notFoundSentinels []string

	r      io.ReadCloser
	o      sync.Once
	runCh  chan struct{}
	runErr error
}

func (e *execReader) setRunErr(err error) {
	e.runErr = err
	close(e.runCh)
}

func (e *execReader) Read(p []byte) (n int, err error) { //nolint:nonamedreturns
	e.o.Do(func() {
		r, err := e.cmd.StdoutPipe()
		if err != nil {
			e.setRunErr(err)
			return
		}
		e.r = r

		go func() {
			err := e.cmd.Run()
			if err != nil {
				for _, sentinel := range e.notFoundSentinels {
					if strings.Contains(err.Error(), sentinel) {
						err = pluginsdk.ErrCacheNotFound
						break
					}
				}
				e.setRunErr(err)
			} else {
				e.setRunErr(err)
			}
		}()
	})

	if e.runErr != nil {
		return 0, io.EOF
	}

	return e.r.Read(p)
}

func (e *execReader) Close() error {
	if e.r == nil {
		return nil
	}

	if e.cmd.Process != nil {
		_ = e.cmd.Process.Kill()
	}

	<-e.runCh

	closeErr := e.r.Close()

	return errors.Join(closeErr, e.runErr)
}

// Get should return engine.ErrCacheNotFound if the key cannot be found, engine.ErrCacheNotFound can also be returned from Close().
func (e Exec) Get(ctx context.Context, key string) (io.ReadCloser, error) {
	cmd := exec.CommandContext(ctx, e.args[0], e.args[1:]...) //nolint:gosec
	cmd.Env = append(cmd.Environ(), "CACHE_KEY="+key)

	return &execReader{
		runCh:             make(chan struct{}),
		notFoundSentinels: e.notFoundSentinels,
		cmd:               cmd,
	}, nil
}
