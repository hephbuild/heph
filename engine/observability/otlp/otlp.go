package otlp

import (
	"context"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/codes"
	"go.opentelemetry.io/otel/trace"
	"heph/engine/artifacts"
	"heph/engine/observability"
	"heph/tgt"
	"os"
	"strings"
)

func targetSpanAttr(t *tgt.Target) trace.SpanStartOption {
	attrs := []attribute.KeyValue{
		{
			Key:   "heph.target",
			Value: attribute.StringValue(t.FQN),
		},
	}

	return trace.WithAttributes(attrs...)
}

func artifactSpanAttr(a artifacts.Artifact) trace.SpanStartOption {
	return trace.WithAttributes(
		attribute.String("heph.artifact_name", a.Name()),
		attribute.String("heph.artifact_display_name", a.DisplayName()),
	)
}

type Hook struct {
	Tracer   trace.Tracer
	RootSpan trace.Span
}

func (h *Hook) newSpan(ctx context.Context, span observability.SpanError, spanName string, opts ...trace.SpanStartOption) (context.Context, observability.Finalizer) {
	_, ctx, fin := h.spanFactory(ctx, span, spanName, opts...)
	return ctx, fin
}

func (h *Hook) spanFactory(ctx context.Context, span observability.SpanError, spanName string, opts ...trace.SpanStartOption) (trace.Span, context.Context, observability.Finalizer) {
	if ctx == nil {
		ctx = context.Background()
	}

	if !trace.SpanFromContext(ctx).SpanContext().IsValid() && h.RootSpan != nil {
		ctx = trace.ContextWithSpan(ctx, h.RootSpan)
	}

	opts = append(opts, trace.WithTimestamp(span.StartTime()))

	ctx, tspan := h.Tracer.Start(ctx, spanName, opts...)
	if observability.IsDuringGen(ctx) {
		tspan.SetAttributes(attribute.Bool("heph.during_gen", true))
	}

	return tspan, ctx, func() {
		err := span.Error()
		if err != nil {
			tspan.RecordError(err)
			tspan.SetStatus(codes.Error, err.Error())
		}
		tspan.End(trace.WithTimestamp(span.EndTime()))
	}
}

func (h *Hook) OnRoot(ctx context.Context, span *observability.BaseSpan) (context.Context, observability.Finalizer) {
	args := append([]string{"heph"}, os.Args[1:]...)
	tspan, ctx, fin := h.spanFactory(ctx, span, strings.Join(args, " "), trace.WithAttributes(attribute.StringSlice("heph.args", args)))
	h.RootSpan = tspan

	return ctx, fin
}

func (h *Hook) OnRun(ctx context.Context, span *observability.TargetSpan) (context.Context, observability.Finalizer) {
	return h.newSpan(ctx, span, "run", targetSpanAttr(span.Target()))
}

func (h *Hook) OnGenPass(ctx context.Context, span *observability.BaseSpan) (context.Context, observability.Finalizer) {
	return h.newSpan(ctx, span, "gen_pass")
}

func (h *Hook) OnCacheDownload(ctx context.Context, span *observability.TargetArtifactCacheSpan) (context.Context, observability.Finalizer) {
	return h.newSpan(ctx, span, "cache_download", targetSpanAttr(span.Target()), artifactSpanAttr(span.Artifact()))
}

func (h *Hook) OnCacheUpload(ctx context.Context, span *observability.TargetArtifactSpan) (context.Context, observability.Finalizer) {
	return h.newSpan(ctx, span, "cache_upload", targetSpanAttr(span.Target()), artifactSpanAttr(span.Artifact()))
}

func (h *Hook) OnRunPrepare(ctx context.Context, span *observability.TargetSpan) (context.Context, observability.Finalizer) {
	return h.newSpan(ctx, span, "run_prepare", targetSpanAttr(span.Target()))
}

func (h *Hook) OnRunExec(ctx context.Context, span *observability.TargetSpan) (context.Context, observability.Finalizer) {
	return h.newSpan(ctx, span, "run_exec", targetSpanAttr(span.Target()))
}

func (h *Hook) OnCollectOutput(ctx context.Context, span *observability.TargetSpan) (context.Context, observability.Finalizer) {
	return h.newSpan(ctx, span, "collect_output", targetSpanAttr(span.Target()))
}

func (h *Hook) OnLocalCacheStore(ctx context.Context, span *observability.TargetSpan) (context.Context, observability.Finalizer) {
	return h.newSpan(ctx, span, "local_cache_store", targetSpanAttr(span.Target()))
}

func (h *Hook) OnLocalCacheCheck(ctx context.Context, span *observability.TargetArtifactCacheSpan) (context.Context, observability.Finalizer) {
	return h.newSpan(ctx, span, "local_cache_check", targetSpanAttr(span.Target()), artifactSpanAttr(span.Artifact()))
}

func (h *Hook) OnExternalCacheGet(ctx context.Context, span *observability.ExternalCacheGetSpan) (context.Context, observability.Finalizer) {
	return h.newSpan(ctx, span, "external_cache_get", targetSpanAttr(span.Target()))
}
