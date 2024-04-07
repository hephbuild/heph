package poolwait

import (
	"context"
	"github.com/hephbuild/heph/utils/xtea"
	"github.com/hephbuild/heph/worker/poolui"
	"github.com/hephbuild/heph/worker2"
)

func termUI(ctx context.Context, name string, deps worker2.Dep, pool *worker2.Engine) error {
	if !xtea.SingleflightTry() {
		return logUI(name, deps, pool)
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
