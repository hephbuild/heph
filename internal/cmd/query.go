package cmd

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/internal/engine"
	"github.com/hephbuild/heph/internal/hbbt/hbbtexec"
	"github.com/hephbuild/heph/internal/hcore/hlog"
	"github.com/hephbuild/heph/plugin/tref"
	"github.com/spf13/cobra"
	"slices"
	"sync"
	"time"
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

var queryCmd = func() *cobra.Command {
	return &cobra.Command{
		Use:     "query",
		Aliases: []string{"q"},
		Args:    cobraArgs(),
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

			rc := engine.GlobalResolveCache

			err = newTermui(ctx, func(ctx context.Context, execFunc func(f hbbtexec.ExecFunc) error) error {
				e, err := newEngine(ctx, root)
				if err != nil {
					return err
				}

				queue, done := renderResults(ctx, execFunc)
				defer done()

				for ref, err := range e.Query(ctx, matcher, rc) {
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
}()

func renderResults(ctx context.Context, execFunc func(f hbbtexec.ExecFunc) error) (func(s string), func()) {
	var m sync.Mutex
	ss := make([]string, 0, 100)
	t := time.NewTicker(20 * time.Millisecond)

	renderRefs := func() {
		m.Lock()
		if len(ss) == 0 {
			m.Unlock()

			return
		}
		pss := slices.Clone(ss)
		ss = ss[:0]
		m.Unlock()

		err := execFunc(func(args hbbtexec.RunArgs) error {
			for _, s := range pss {
				fmt.Fprintln(args.Stdout, s)
			}

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
				renderRefs()
				return
			default:
				select {
				case <-closeCh:
					renderRefs()
					return
				case <-t.C:
					renderRefs()
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

func init() {
	rootCmd.AddCommand(queryCmd)
}
