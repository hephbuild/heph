package bootstrap

import (
	"context"
	"github.com/hephbuild/heph/observability"
	obotlp "github.com/hephbuild/heph/observability/otlp"
	"github.com/hephbuild/heph/utils/finalizers"
	"go.opentelemetry.io/otel/exporters/jaeger"
	"go.opentelemetry.io/otel/sdk/resource"
	tracesdk "go.opentelemetry.io/otel/sdk/trace"
	semconv "go.opentelemetry.io/otel/semconv/v1.12.0"
)

func setupJaeger(fins *finalizers.Finalizers, obs *observability.Observability, endpoint string) error {
	if endpoint == "" {
		return nil
	}

	opts := []tracesdk.TracerProviderOption{
		tracesdk.WithResource(resource.NewWithAttributes(
			semconv.SchemaURL,
			semconv.ServiceNameKey.String("heph"),
		)),
	}

	jexp, err := jaeger.New(jaeger.WithCollectorEndpoint(jaeger.WithEndpoint(endpoint)))
	if err != nil {
		return err
	}

	opts = append(opts, tracesdk.WithBatcher(jexp))

	pr := tracesdk.NewTracerProvider(opts...)

	hook := &obotlp.Hook{
		Tracer: pr.Tracer("heph"),
	}

	obs.RegisterHook(hook)

	ctx := context.Background()

	fins.Register(func() {
		_ = pr.ForceFlush(ctx)
		_ = pr.Shutdown(ctx)
	})

	return nil
}
