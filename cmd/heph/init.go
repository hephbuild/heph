package main

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/bootstrap"
	"github.com/hephbuild/heph/engine"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/worker/poolwait"
	"os"
	"strings"
	"time"
)

type PreRunOpts struct {
	Engine       *engine.Engine
	PoolWaitName string
	LinkAll      bool
}

func bootstrapOptions() (bootstrap.BootOpts, error) {
	paramsm := make(map[string]string, len(*params))
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
		FlowID:                strings.TrimSpace(os.Getenv("HEPH_FLOW_ID")),
	}, nil
}

var engineAlreadyInit bool

func engineInit(ctx context.Context, postBoot func(bootstrap.BaseBootstrap) error) (bootstrap.EngineBootstrap, error) {
	if engineAlreadyInit {
		panic("cannot call engineInit multiple times")
	}
	engineAlreadyInit = true

	opts, err := bootstrapOptions()
	if err != nil {
		return bootstrap.EngineBootstrap{}, err
	}

	// This allows to block reading stdin right after a potential CheckAndUpgrade has run
	opts.PostBootBase = postBoot

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

		bs.Pool.Stop(err)
	})

	return bs, nil
}

func preRunWithGen(ctx context.Context) (bootstrap.EngineBootstrap, error) {
	bs, err := engineInit(ctx, nil)
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

	err = poolwait.Wait(ctx, opts.PoolWaitName, e.Pool, deps, *plain)
	if err != nil {
		return err
	}

	return nil
}

func preRunAutocomplete(ctx context.Context, includePrivate bool) (targetspec.TargetSpecs, []string, error) {
	bs, err := engineInit(ctx, nil)
	if err != nil {
		return nil, nil, err
	}

	return preRunAutocompleteWithBootstrap(ctx, bs, includePrivate)
}

func preRunAutocompleteWithBootstrap(ctx context.Context, bs bootstrap.EngineBootstrap, includePrivate bool) (targetspec.TargetSpecs, []string, error) {
	err := preRunWithGenWithOpts(ctx, PreRunOpts{Engine: bs.Engine})
	if err != nil {
		return nil, nil, err
	}

	targets := bs.Engine.Graph.Targets()
	if !includePrivate {
		targets = targets.Public()
	}

	return targets.Specs(), bs.Engine.Graph.Labels().Slice(), nil
}
