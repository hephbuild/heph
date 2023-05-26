package main

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/bootstrap"
	"github.com/hephbuild/heph/engine"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/worker/poolui"
	"strings"
	"time"
)

type PreRunOpts struct {
	Engine       *engine.Engine
	PoolWaitName string
	LinkAll      bool
}

func bootstrapOptions() (bootstrap.BootOpts, error) {
	paramsm := map[string]string{}
	for _, s := range *params {
		parts := strings.SplitN(s, "=", 2)
		if len(parts) != 2 {
			return bootstrap.BootOpts{}, fmt.Errorf("parameter must be name=value, got `%v`", s)
		}

		paramsm[parts[0]] = parts[1]
	}

	return bootstrap.BootOpts{
		Profiles:              *profiles,
		Workers:               workers,
		Params:                paramsm,
		Summary:               *summary || *summaryGen,
		JaegerEndpoint:        *jaegerEndpoint,
		DisableCloudTelemetry: *noCloudTelemetry,
	}, nil
}

var engineAlreadyInit bool

func engineInit(ctx context.Context) (bootstrap.EngineBootstrap, error) {
	if engineAlreadyInit {
		panic("cannot call engineInit multiple times")
	}
	engineAlreadyInit = true

	opts, err := bootstrapOptions()
	if err != nil {
		return bootstrap.EngineBootstrap{}, err
	}

	bs, err := bootstrap.BootWithEngine(ctx, opts)
	if err != nil {
		return bs, err
	}

	Finalizers.RegisterWithErr(func(err error) {
		bs.Finalizers.Run(err)

		if !bs.Engine.Pool.IsDone() {
			log.Tracef("Waiting for all pool items to finish")
			select {
			case <-bs.Engine.Pool.Done():
			case <-time.After(time.Second):
				log.Infof("Waiting for background jobs to finish...")
				<-bs.Engine.Pool.Done()
			}
			log.Tracef("All pool items finished")

			bs.Engine.Pool.Stop(nil)

			err := bs.Engine.Pool.Err()
			if err != nil {
				log.Error(err)
			}
		}

		if bs.Summary != nil {
			PrintSummary(bs.Summary, *summaryGen)
		}
	})

	return bs, nil
}

func bootstrapInit(ctx context.Context) (bootstrap.Bootstrap, error) {
	opts, err := bootstrapOptions()
	if err != nil {
		return bootstrap.Bootstrap{}, err
	}

	bs, err := bootstrap.Boot(ctx, opts)
	if err != nil {
		return bootstrap.Bootstrap{}, err
	}

	Finalizers.RegisterWithErr(func(err error) {
		bs.Finalizers.Run(err)
	})

	return bs, nil
}

func preRunWithGen(ctx context.Context) (bootstrap.EngineBootstrap, error) {
	bs, err := engineInit(ctx)
	if err != nil {
		return bs, err
	}

	err = preRunWithGenWithOpts(ctx, PreRunOpts{
		Engine: bs.Engine,
	})
	if err != nil {
		return bs, err
	}

	return bs, err
}

func preRunWithGenWithOpts(ctx context.Context, opts PreRunOpts) error {
	e := opts.Engine
	if e == nil {
		bs, err := engineInit(ctx)
		if err != nil {
			return err
		}
		e = bs.Engine
	}

	if *noGen {
		log.Info("Generated targets disabled")
		if opts.LinkAll {
			err := e.Graph.LinkTargets(ctx, true, nil)
			if err != nil {
				return fmt.Errorf("linking %w", err)
			}
		}

		return nil
	}

	deps, err := e.ScheduleGenPass(ctx, opts.LinkAll)
	if err != nil {
		return err
	}

	if opts.PoolWaitName == "" {
		opts.PoolWaitName = "PreRun gen"
	}

	err = poolui.Wait(opts.PoolWaitName, e.Pool, deps, *plain)
	if err != nil {
		return err
	}

	return nil
}

func preRunAutocomplete(ctx context.Context, includePrivate bool) (targetspec.TargetSpecs, []string, error) {
	bs, err := engineInit(ctx)
	if err != nil {
		return nil, nil, err
	}

	cache, err := bs.Engine.LoadAutocompleteCache()
	if err != nil {
		log.Warnf("autocomplete cache: %v", err)
	}

	if cache != nil {
		targets := cache.Targets
		if !includePrivate {
			targets = cache.PublicTargets()
		}

		return targets, cache.Labels(), nil
	}

	err = preRunWithGenWithOpts(ctx, PreRunOpts{Engine: bs.Engine})
	if err != nil {
		return nil, nil, err
	}

	targets := bs.Engine.Graph.Targets()
	if !includePrivate {
		targets = targets.Public()
	}

	return targets.Specs(), bs.Engine.Graph.Labels().Slice(), nil
}
