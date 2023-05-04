package main

import (
	"context"
	"fmt"
	"github.com/hephbuild/heph/cloudclient"
	"github.com/hephbuild/heph/engine"
	obhephcloud "github.com/hephbuild/heph/engine/observability/hephcloud"
	obotlp "github.com/hephbuild/heph/engine/observability/otlp"
	obsummary "github.com/hephbuild/heph/engine/observability/summary"
	"github.com/hephbuild/heph/log/log"
	"github.com/hephbuild/heph/targetspec"
	"github.com/hephbuild/heph/worker"
	"go.opentelemetry.io/otel/exporters/jaeger"
	"go.opentelemetry.io/otel/sdk/resource"
	tracesdk "go.opentelemetry.io/otel/sdk/trace"
	semconv "go.opentelemetry.io/otel/semconv/v1.12.0"
	"os"
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

	e.Config.Profiles = *profiles

	err := e.Init(ctx)
	if err != nil {
		return err
	}

	if *jaegerEndpoint != "" {
		opts := []tracesdk.TracerProviderOption{
			tracesdk.WithResource(resource.NewWithAttributes(
				semconv.SchemaURL,
				semconv.ServiceNameKey.String("heph"),
			)),
		}

		jexp, err := jaeger.New(jaeger.WithCollectorEndpoint(jaeger.WithEndpoint(*jaegerEndpoint)))
		if err != nil {
			return err
		}

		opts = append(opts, tracesdk.WithBatcher(jexp))

		pr := tracesdk.NewTracerProvider(opts...)

		hook := &obotlp.Hook{
			Tracer: pr.Tracer("heph"),
		}

		e.Observability.RegisterHook(hook)

		e.RegisterExitHandler(func() {
			_ = pr.ForceFlush(context.Background())
			_ = pr.Shutdown(context.Background())
		})
	}

	if e.Config.Cloud.URL != "" && e.Config.Cloud.Project != "" {
		cloudClient := cloudclient.New(strings.TrimRight(e.Config.Cloud.URL, "/") + "/api/graphql")
		e.CloudClient = &cloudClient

		token := strings.TrimSpace(os.Getenv("HEPH_CLOUD_TOKEN"))
		if token == "" {
			data, err := e.GetCloudAuthData()
			if err != nil {
				return fmt.Errorf("cloud auth: %w", err)
			}

			if data != nil {
				token = data.Token
			}
		}

		if token == "" {
			log.Errorf("You must login to use cloud features")
		} else {
			client := cloudClient.WithAuthToken(token)
			e.CloudClientAuth = &client
			e.CloudToken = token

			if !*noCloudTelemetry {
				_, err := cloudclient.AuthActor(ctx, client)
				if err != nil {
					log.Errorf("You must login to use cloud features: auth error: %v", err)
				} else {
					hook := obhephcloud.NewHook(&obhephcloud.Hook{
						Client:    client,
						ProjectID: e.Config.Cloud.Project,
						Config:    e.Config,
					})
					e.Observability.RegisterHook(hook)
					e.GetFlowID = hook.GetFlowID

					flush := hook.Start(ctx)
					e.RegisterExitHandler(func() {
						flush()
					})
				}
			}
		}
	}

	if *summary || *summaryGen {
		e.Summary = &obsummary.Summary{}
		e.Observability.RegisterHook(e.Summary)
	}

	ctx, rootSpan := e.Observability.SpanRoot(ctx)
	e.RegisterExitHandlerWithErr(func(err error) {
		rootSpan.EndError(err)
	})

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

func preRunWithGen(ctx context.Context) error {
	err := engineInit(ctx)
	if err != nil {
		return err
	}

	err = preRunWithGenWithOpts(ctx, PreRunOpts{
		Engine: Engine,
	})
	if err != nil {
		return err
	}

	return nil
}

type PreRunOpts struct {
	Engine       *engine.Engine
	PoolWaitName string
	LinkAll      bool
}

func preRunWithGenWithOpts(ctx context.Context, opts PreRunOpts) error {
	e := opts.Engine
	if e == nil {
		err := engineInit(ctx)
		if err != nil {
			return err
		}
		e = Engine
	} else {
		err := engineInitWithEngine(ctx, e)
		if err != nil {
			return err
		}
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

	err = WaitPool(opts.PoolWaitName, e.Pool, deps)
	if err != nil {
		return err
	}

	return nil
}

func preRunAutocomplete(ctx context.Context) (targetspec.TargetSpecs, []string, error) {
	return preRunAutocompleteInteractive(ctx, false, true)
}

func preRunAutocompleteInteractive(ctx context.Context, includePrivate, silent bool) (targetspec.TargetSpecs, []string, error) {
	err := engineInit(ctx)
	if err != nil {
		return nil, nil, err
	}

	cache, err := Engine.LoadAutocompleteCache()
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

	err = preRunWithGen(ctx)
	if err != nil {
		return nil, nil, err
	}

	targets := Engine.Targets
	if !includePrivate {
		targets = targets.Public()
	}

	return targets.Specs(), Engine.Labels.Slice(), nil
}
