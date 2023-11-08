package main

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/bootstrap"
	"github.com/hephbuild/heph/graph"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/scheduler"
	"github.com/hephbuild/heph/specs"
	"github.com/hephbuild/heph/targetrun"
)

func parseTargetFromArgs(ctx context.Context, args []string) (bootstrap.SchedulerBootstrap, *graph.Target, error) {
	if len(args) != 1 {
		return bootstrap.SchedulerBootstrap{}, nil, fmt.Errorf("expected one arg")
	}

	bs, err := schedulerInit(ctx, func(bootstrap.BaseBootstrap) error {
		return bootstrap.BlockReadStdin(args)
	})
	if err != nil {
		return bs, nil, err
	}

	rrs, err := parseTargetsAndArgsWithScheduler(ctx, bs.Scheduler, args, parseTargetsAndArgsOptions{
		useStdin:     false,
		onlyExplicit: true,
	})
	if err != nil {
		return bs, nil, err
	}

	if len(rrs) != 1 {
		log.Debugf("no rrs: %v", args)

		s := ""
		if len(args) > 0 {
			s = args[0]
		}
		return bs, nil, specs.NewTargetNotFoundError(s, bs.Graph.Targets())
	}

	return bs, rrs[0].Target, nil
}

func parseTargetsAndArgs(ctx context.Context, args []string, matcherFilter func(m specs.Matcher) error) (bootstrap.SchedulerBootstrap, targetrun.Requests, error) {
	bs, err := schedulerInit(ctx, func(bootstrap.BaseBootstrap) error {
		return bootstrap.BlockReadStdin(args)
	})
	if err != nil {
		return bootstrap.SchedulerBootstrap{}, nil, err
	}

	rrs, err := parseTargetsAndArgsWithScheduler(ctx, bs.Scheduler, args, parseTargetsAndArgsOptions{
		useStdin:      true,
		onlyExplicit:  false,
		matcherFilter: matcherFilter,
	})
	if err != nil {
		return bootstrap.SchedulerBootstrap{}, nil, err
	}

	return bs, rrs, err
}

type parseTargetsAndArgsOptions struct {
	useStdin      bool
	onlyExplicit  bool
	matcherFilter func(m specs.Matcher) error
}

func parseTargetsAndArgsWithScheduler(ctx context.Context, e *scheduler.Scheduler, args []string, opts parseTargetsAndArgsOptions) (targetrun.Requests, error) {
	m, targs, err := bootstrap.ParseTargetAddrsAndArgs(args, opts.useStdin)
	if err != nil {
		return nil, err
	}

	if opts.matcherFilter != nil {
		err := opts.matcherFilter(m)
		if err != nil {
			return nil, err
		}
	}

	if opts.onlyExplicit {
		if !specs.IsMatcherExplicit(m) {
			return nil, fmt.Errorf("only explicit selector allowed (only exact target)")
		}
	}

	return generateRRs(ctx, e, m, targs)
}

func generateRRs(ctx context.Context, e *scheduler.Scheduler, m specs.Matcher, targs []string) (targetrun.Requests, error) {
	return bootstrap.GenerateRRs(ctx, e, m, targs, getRROpts(), *plain)
}
