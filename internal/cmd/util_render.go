package cmd

import (
	"context"
	"fmt"
	"sync"
	"sync/atomic"
	"time"

	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	"github.com/hephbuild/heph/internal/hcore/hlog"
)

func render(ctx context.Context, execFunc func(f hbbtexec.ExecFunc) error) (func(s string), func()) {
	var m sync.Mutex
	ss := make([]string, 0, 100)
	t := time.NewTicker(20 * time.Millisecond)

	var pending atomic.Bool

	renderRefs := func(force bool) {
		if !force {
			m.Lock()
			empty := len(ss) == 0
			m.Unlock()
			if empty {
				return
			}

			if !pending.CompareAndSwap(false, true) {
				return
			}
		}

		err := execFunc(func(args hbbtexec.RunArgs) error {
			defer pending.Swap(false)

			m.Lock()
			defer m.Unlock()

			if len(ss) == 0 {
				return nil
			}

			for _, s := range ss {
				_, _ = fmt.Fprintln(args.Stdout, s)
			}

			ss = ss[:0]

			return nil
		})
		if err != nil {
			hlog.From(ctx).Error(fmt.Sprintf("exec: %v", err))
		}
	}

	closeCh := make(chan struct{})
	bgDoneCh := make(chan struct{})
	go func() {
		defer close(bgDoneCh)
		for {
			select {
			case <-closeCh:
				renderRefs(true)
				return
			default:
				select {
				case <-closeCh:
					renderRefs(true)
					return
				case <-t.C:
					renderRefs(false)
				}
			}
		}
	}()

	return func(s string) {
			m.Lock()
			ss = append(ss, s)
			m.Unlock()
		}, func() {
			t.Stop()
			close(closeCh)
			<-bgDoneCh
		}
}
