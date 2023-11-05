package poolwait

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/utils/xtea"
	"github.com/hephbuild/heph/worker"
	"github.com/hephbuild/heph/worker/poolui"
)

func termUI(ctx context.Context, name string, deps *worker.WaitGroup, pool *worker.Pool) error {
	if !xtea.SingleflightTry() {
		return logUI(name, deps, pool)
	}

	defer xtea.SingleflightDone()

	m := poolui.New(ctx, name, deps, pool, true)

	err := xtea.RunModel(m)
	if err != nil {
		return err
	}

	if !deps.IsDone() {
		pool.Stop(fmt.Errorf("TUI exited unexpectedly"))
	}

	return nil
}
