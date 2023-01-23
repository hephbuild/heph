package cmd

import (
	"context"
	"fmt"
	"go.opentelemetry.io/otel/exporters/jaeger"
	"go.opentelemetry.io/otel/sdk/resource"
	tracesdk "go.opentelemetry.io/otel/sdk/trace"
	semconv "go.opentelemetry.io/otel/semconv/v1.12.0"
	"heph/engine"
	log "heph/hlog"
	"heph/worker"
	"strings"
)

func engineFactory() (*engine.Engine, error) {
	root, err := findRoot()
	if err != nil {
		return nil, err
	}

	log.Tracef("Root: %v", root)

	e := engine.New(root)

	return e, nil
}

func engineInit(ctx context.Context) error {
	if Engine == nil {
		var err error
		Engine, err = engineFactory()
		if err != nil {
			return err
		}
	}

	return engineInitWithEngine(ctx, Engine)
}

func engineInitWithEngine(ctx context.Context, e *engine.Engine) error {
	if e.RanInit {
		return nil
	}
	e.RanInit = true

	if *summary || *summaryGen || *jaegerEndpoint != "" {
		opts := []tracesdk.TracerProviderOption{
			tracesdk.WithResource(resource.NewWithAttributes(
				semconv.SchemaURL,
				semconv.ServiceNameKey.String("heph"),
			)),
		}

		if *jaegerEndpoint != "" {
			jexp, err := jaeger.New(jaeger.WithCollectorEndpoint(jaeger.WithEndpoint(*jaegerEndpoint)))
			if err != nil {
				return err
			}

			opts = append(opts, tracesdk.WithBatcher(jexp))
		}

		if *summary || *summaryGen {
			opts = append(opts, tracesdk.WithSpanProcessor(e.Stats))
		}

		pr := tracesdk.NewTracerProvider(opts...)
		e.Tracer = pr.Tracer("heph")

		e.StartRootSpan()
		e.RegisterExitHandler(func() {
			e.RootSpan.End()
			_ = pr.ForceFlush(context.Background())
			_ = pr.Shutdown(context.Background())
		})
	}

	e.Config.Profiles = *profiles

	err := e.Init()
	if err != nil {
		return err
	}

	paramsm := map[string]string{}
	for k, v := range e.Config.Params {
		paramsm[k] = v
	}
	for _, s := range *params {
		parts := strings.SplitN(s, "=", 2)
		if len(parts) != 2 {
			return fmt.Errorf("parameter must be name=value, got `%v`", s)
		}

		paramsm[parts[0]] = parts[1]
	}
	e.Params = paramsm
	e.Pool = worker.NewPool(workers)
	e.RegisterExitHandler(func() {
		e.Pool.Stop(nil)
	})

	err = e.Parse(ctx)
	if err != nil {
		return err
	}

	return nil
}

func preRunWithGen(ctx context.Context, silent bool) error {
	err := engineInit(ctx)
	if err != nil {
		return err
	}

	err = preRunWithGenWithOpts(ctx, PreRunOpts{
		Engine: Engine,
		Silent: silent,
	})
	if err != nil {
		return err
	}

	return nil
}

type PreRunOpts struct {
	Engine       *engine.Engine
	Silent       bool
	PoolWaitName string
	LinkAll      bool
}

func preRunWithGenWithOpts(ctx context.Context, opts PreRunOpts) error {
	e := opts.Engine

	err := engineInitWithEngine(ctx, e)
	if err != nil {
		return err
	}

	if *noGen {
		log.Info("Generated targets disabled")
		if opts.LinkAll {
			err := e.LinkTargets(ctx, true, nil)
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

	err = WaitPool(opts.PoolWaitName, e.Pool, deps, opts.Silent)
	if err != nil {
		return err
	}

	return nil
}

func preRunAutocomplete(ctx context.Context) ([]string, []string, error) {
	return preRunAutocompleteInteractive(ctx, false, true)
}

func preRunAutocompleteInteractive(ctx context.Context, includePrivate, silent bool) ([]string, []string, error) {
	err := engineInit(ctx)
	if err != nil {
		return nil, nil, err
	}

	cache, _ := Engine.LoadAutocompleteCache()

	if cache != nil {
		if includePrivate {
			return cache.AllTargets, cache.Labels, nil
		}

		return cache.PublicTargets, cache.Labels, nil
	}

	err = preRunWithGen(ctx, silent)
	if err != nil {
		return nil, nil, err
	}

	targets := Engine.Targets
	if !all {
		targets = targets.Public()
	}

	return targets.FQNs(), Engine.Labels.Slice(), nil
}
