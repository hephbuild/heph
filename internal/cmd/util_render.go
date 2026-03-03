package cmd

import (
	"context"
	"fmt"
	"io"
	"runtime"
	"sync"
	"time"

	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	"github.com/hephbuild/heph/internal/hcore/hlog"
)

func render(ctx context.Context, execFunc func(f hbbtexec.ExecFunc) error) (func(s string), func()) {
	var m sync.Mutex
	ss := make([]string, 0, 100)
	t := time.NewTicker(50 * time.Millisecond)

	innerRender := func(w io.Writer) bool {
		m.Lock()
		defer m.Unlock()
		if len(ss) == 0 {
			return false
		}

		for _, s := range ss {
			_, _ = fmt.Fprintln(w, s)
		}
		ss = ss[:0]

		return true
	}

	execRender := func() {
		m.Lock()
		empty := len(ss) == 0
		m.Unlock()

		if empty {
			return
		}

		err := execFunc(func(args hbbtexec.RunArgs) error {
			for {
				if !innerRender(args.Stdout) { //nolint:staticcheck
					break
				}
				runtime.Gosched()
			}

			return nil
		})
		if err != nil {
			hlog.From(ctx).Error(fmt.Sprintf("exec: %v", err))
		}
	}

	closeCh := make(chan struct{})
	var wg sync.WaitGroup
	wg.Go(func() {
		defer execRender()

		for {
			select {
			case <-closeCh:
				return
			default:
				select {
				case <-closeCh:
					return
				case <-t.C:
					execRender()
				}
			}
		}
	})

	return func(s string) {
			m.Lock()
			ss = append(ss, s)
			m.Unlock()
		}, func() {
			t.Stop()
			close(closeCh)
			wg.Wait()
		}
}
