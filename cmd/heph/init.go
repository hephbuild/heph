package main

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/bootstrap"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/scheduler"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/utils/tuistatus"
	"os"
	"strings"
	"time"
)

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

var schedulerAlreadyInit bool

func schedulerInit(ctx context.Context, postBoot func(bootstrap.BaseBootstrap) error) (bootstrap.SchedulerBootstrap, error) {
	if schedulerAlreadyInit {
		panic("cannot call schedulerInit multiple times")
	}
	schedulerAlreadyInit = true

	opts, err := bootstrapOptions()
	if err != nil {
		return bootstrap.SchedulerBootstrap{}, err
	}

	// This allows to block reading stdin right after a potential CheckAndUpgrade has run
	opts.PostBootBase = postBoot

	bs, err := bootstrap.BootWithScheduler(ctx, opts)
	if err != nil {
		return bs, err
	}

	Finalizers.RegisterWithErr(func(err error) {
		bs.Finalizers.Run(err)

		gb := bs.Scheduler.BackgroundTracker.Group()
		bs.Pool.Schedule(gb)
		if !gb.GetState().IsFinal() {
			log.Tracef("Waiting for all pool items to finish")
			select {
			case <-gb.Wait():
			case <-time.After(time.Second):
				log.Infof("Waiting for background jobs to finish...")
				<-gb.Wait()
			}
			log.Tracef("All pool items finished")
		}

		if bs.Summary != nil {
			PrintSummary(bs.Summary, *summaryGen)
		}
	})

	return bs, nil
}

func bootstrapBase(ctx context.Context) (bootstrap.BaseBootstrap, error) {
	opts, err := bootstrapOptions()
	if err != nil {
		return bootstrap.BaseBootstrap{}, err
	}

	return bootstrap.BootBase(ctx, opts)
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

func schedulerWithGenInit(ctx context.Context) (bootstrap.SchedulerBootstrap, error) {
	bs, err := schedulerInit(ctx, nil)
	if err != nil {
		return bs, err
	}

	if !*noGen {
		err := bootstrap.RunAllGen(ctx, bs.Scheduler, *plain)
		if err != nil {
			return bs, err
		}
	}

	return bs, err
}

func linkAll(ctx context.Context, e *scheduler.Scheduler) error {
	if !*noGen {
		err := bootstrap.RunAllGen(ctx, e, *plain)
		if err != nil {
			return err
		}
	}

	err := tuistatus.DoE(ctx, func(ctx context.Context) error {
		err := e.Graph.LinkTargets(ctx, *noGen, nil, true)
		if err != nil {
			return fmt.Errorf("linking: %w", err)
		}

		return nil
	})
	if err != nil {
		return err
	}

	return nil
}

func autocompleteInit(ctx context.Context, includePrivate bool) (specs.Targets, []string, error) {
	bs, err := schedulerInit(ctx, nil)
	if err != nil {
		return nil, nil, err
	}

	return autocompleteInitWithBootstrap(ctx, bs, includePrivate)
}

func autocompleteInitWithBootstrap(ctx context.Context, bs bootstrap.SchedulerBootstrap, includePrivate bool) (specs.Targets, []string, error) {
	err := bootstrap.RunAllGen(ctx, bs.Scheduler, *plain)
	if err != nil {
		return nil, nil, err
	}

	targets := bs.Scheduler.Graph.Targets()
	if !includePrivate {
		targets = targets.Public()
	}

	return targets.Specs(), bs.Scheduler.Graph.Labels().Slice(), nil
}
