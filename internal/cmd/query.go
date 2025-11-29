package cmd

import (
	"context"
	"fmt"
	"sync"
	"sync/atomic"
	"time"

	"github.com/hephbuild/heph/internal/tmatch"
	"github.com/hephbuild/heph/lib/tref"

	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/spf13/cobra"
)

// heph r :lint          run //some:lint (assuming pwd = some)
// heph r :lint .        run //some:lint (assuming pwd = some)
// heph r //other:lint   run //other:lint
// heph r lint //...     run all lint targets

// heph r lint ./...     run all test target under cwd
// heph r lint ./foo/... run all test target under foo
// heph r lint .

// heph r //:lint

// heph r -e 'lint && //some/**/*'
// heph r -e 'test || (k8s-validate && !k8s-validate-prod)'

var queryCmd *cobra.Command

func init() {
	var ignore []string

	queryCmd = &cobra.Command{
		Use:     "query",
		Aliases: []string{"q"},
		Args:    parseMatcherCobraArgs(),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx := cmd.Context()

			ctx, stop := newSignalNotifyContext(ctx)
			defer stop()

			cwd, err := engine.Cwd()
			if err != nil {
				return err
			}

			root, err := engine.Root()
			if err != nil {
				return err
			}

			matcher, err := parseMatcher(args, cwd, root)
			if err != nil {
				return err
			}

			for _, s := range ignore {
				ignoreMatcher, err := tmatch.ParsePackageMatcher(s, cwd, root)
				if err != nil {
					return err
				}

				matcher = tmatch.And(matcher, tmatch.Not(ignoreMatcher))
			}

			err = newTermui(ctx, func(ctx context.Context, execFunc func(f hbbtexec.ExecFunc) error) error {
				e, err := newEngine(ctx, root)
				if err != nil {
					return err
				}

				rs, cleanRs := e.NewRequestState()
				defer cleanRs()

				queue, flushResults := renderResults(ctx, execFunc)
				defer flushResults()

				for ref, err := range e.Query(ctx, rs, matcher) {
					if err != nil {
						return err
					}

					if err := ctx.Err(); err != nil {
						return err
					}

					queue(tref.Format(ref))
				}

				return nil
			})
			if err != nil {
				return err
			}

			return nil
		},
	}

	queryCmd.Flags().StringArrayVar(&ignore, "ignore", nil, "Filter universe of targets")

	rootCmd.AddCommand(queryCmd)
}

func renderResults(ctx context.Context, execFunc func(f hbbtexec.ExecFunc) error) (func(s string), func()) {
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
				fmt.Fprintln(args.Stdout, s)
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
