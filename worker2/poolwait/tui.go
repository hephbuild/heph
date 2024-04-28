package poolwait

import (
	"context"
	"github.com/hephbuild/heph/utils/xtea"
	"github.com/hephbuild/heph/worker2"
	"github.com/hephbuild/heph/worker2/poolui"
	"time"
)

func termUI(ctx context.Context, name string, deps worker2.Dep, pool *worker2.Engine) error {
	if !xtea.SingleflightTry() {
		return logUI(name, deps, pool, time.Second)
	}

	defer xtea.SingleflightDone()

	m := poolui.New(ctx, name, deps, pool, true)
	defer m.Clean()

	err := xtea.RunModel(m)
	if err != nil {
		return err
	}

	return nil
}
