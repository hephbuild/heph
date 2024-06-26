package poolwait

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils/xtea"
	"github.com/hephbuild/heph/worker2"
	"go.uber.org/multierr"
	"os"
	"strconv"
	"time"
)

var debug bool

func init() {
	debug, _ = strconv.ParseBool(os.Getenv("HEPH_DEBUG_POOLWAIT"))
}

func Wait(ctx context.Context, name string, pool *worker2.Engine, deps worker2.Dep, plain bool, interval time.Duration) error {
	pool.Schedule(deps)

	if debug {
		stopServer, err := Server(deps)
		if err != nil {
			return err
		}
		defer stopServer()
	}

	useTUI := xtea.IsTerm() && !plain

	log.Tracef("WaitPool %v", name)
	defer func() {
		log.Tracef("WaitPool %v DONE", name)
	}()

	if useTUI {
		err := termUI(ctx, name, deps, pool)
		if err != nil {
			return fmt.Errorf("poolui: %w", err)
		}
	} else {
		err := logUI(name, deps, pool, interval)
		if err != nil {
			return fmt.Errorf("logpoolui: %w", err)
		}
	}

	err := deps.GetErr()

	return multierr.Combine(worker2.CollectRootErrors(err)...)
}
