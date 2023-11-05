package poolwait

import (
	"context"
	"errors"
	"fmt"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/utils/xtea"
	"github.com/hephbuild/heph/worker"
	"go.uber.org/multierr"
)

func Wait(ctx context.Context, name string, pool *worker.Pool, deps *worker.WaitGroup, plain bool) error {
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
		err := logUI(name, deps, pool)
		if err != nil {
			return fmt.Errorf("logpoolui: %w", err)
		}
	}

	perr := pool.Err()
	derr := deps.Err()

	if perr != nil && derr != nil {
		if errors.Is(perr, derr) || errors.Is(derr, perr) || derr == perr {
			return perr
		}

		perr = fmt.Errorf("pool: %w", perr)
		derr = fmt.Errorf("deps: %w", derr)
	}

	return multierr.Combine(perr, derr)
}
