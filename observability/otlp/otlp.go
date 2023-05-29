package otlp

import (
	"context"
	"github.com/hephbuild/heph/artifacts"
	observability2 "github.com/hephbuild/heph/observability"
	"github.com/hephbuild/heph/tgt"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/codes"
	"go.opentelemetry.io/otel/trace"
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
	observability2.BaseHook
	Tracer   trace.Tracer
	RootSpan trace.Span
}

func (h *Hook) newSpan(ctx context.Context, span observability2.SpanError, spanName string, opts ...trace.SpanStartOption) (context.Context, observability2.SpanHook) {
	_, ctx, fin := h.spanFactory(ctx, span, spanName, opts...)
	return ctx, fin
}

func (h *Hook) spanFactory(ctx context.Context, span observability2.SpanError, spanName string, opts ...trace.SpanStartOption) (trace.Span, context.Context, observability2.SpanHook) {
	if ctx == nil {
		ctx = context.Background()
	}

	if !trace.SpanFromContext(ctx).SpanContext().IsValid() && h.RootSpan != nil {
		ctx = trace.ContextWithSpan(ctx, h.RootSpan)
	}

	opts = append(opts, trace.WithTimestamp(span.StartTime()))

	ctx, tspan := h.Tracer.Start(ctx, spanName, opts...)
	if observability2.IsDuringGen(ctx) {
		tspan.SetAttributes(attribute.Bool("heph.during_gen", true))
	}

	if !span.ScheduledTime().IsZero() {
		tspan.AddEvent("heph.scheduled", trace.WithTimestamp(span.ScheduledTime()))
	}
	if !span.QueuedTime().IsZero() {
		tspan.AddEvent("heph.queued", trace.WithTimestamp(span.QueuedTime()))
	}

	return tspan, ctx, observability2.FinalizerSpanHook(func() {
		err := span.Error()
		if err != nil {
			tspan.RecordError(err)
			tspan.SetStatus(codes.Error, err.Error())
		}
		tspan.End(trace.WithTimestamp(span.EndTime()))
	})
}

func (h *Hook) OnRoot(ctx context.Context, span *observability2.BaseSpan) (context.Context, observability2.SpanHook) {
	args := append([]string{"heph"}, os.Args[1:]...)
	tspan, ctx, fin := h.spanFactory(ctx, span, strings.Join(args, " "), trace.WithAttributes(attribute.StringSlice("heph.args", args)))
	h.RootSpan = tspan

	return ctx, fin
}

func (h *Hook) OnRun(ctx context.Context, span *observability2.TargetSpan) (context.Context, observability2.SpanHook) {
	return h.newSpan(ctx, span, "run", targetSpanAttr(span.Target()))
}

func (h *Hook) OnGenPass(ctx context.Context, span *observability2.BaseSpan) (context.Context, observability2.SpanHook) {
	return h.newSpan(ctx, span, "gen_pass")
}

func (h *Hook) OnCacheDownload(ctx context.Context, span *observability2.TargetArtifactCacheSpan) (context.Context, observability2.SpanHook) {
	return h.newSpan(ctx, span, "cache_download", targetSpanAttr(span.Target()), artifactSpanAttr(span.Artifact()))
}

func (h *Hook) OnCacheUpload(ctx context.Context, span *observability2.TargetArtifactCacheSpan) (context.Context, observability2.SpanHook) {
	return h.newSpan(ctx, span, "cache_upload", targetSpanAttr(span.Target()), artifactSpanAttr(span.Artifact()))
}

func (h *Hook) OnRunPrepare(ctx context.Context, span *observability2.TargetSpan) (context.Context, observability2.SpanHook) {
	return h.newSpan(ctx, span, "run_prepare", targetSpanAttr(span.Target()))
}

func (h *Hook) OnRunExec(ctx context.Context, span *observability2.TargetExecSpan) (context.Context, observability2.SpanHook) {
	return h.newSpan(ctx, span, "run_exec", targetSpanAttr(span.Target()))
}

func (h *Hook) OnCollectOutput(ctx context.Context, span *observability2.TargetSpan) (context.Context, observability2.SpanHook) {
	return h.newSpan(ctx, span, "collect_output", targetSpanAttr(span.Target()))
}

func (h *Hook) OnLocalCacheStore(ctx context.Context, span *observability2.TargetSpan) (context.Context, observability2.SpanHook) {
	return h.newSpan(ctx, span, "local_cache_store", targetSpanAttr(span.Target()))
}

func (h *Hook) OnLocalCacheCheck(ctx context.Context, span *observability2.TargetArtifactCacheSpan) (context.Context, observability2.SpanHook) {
	return h.newSpan(ctx, span, "local_cache_check", targetSpanAttr(span.Target()), artifactSpanAttr(span.Artifact()))
}

func (h *Hook) OnExternalCacheGet(ctx context.Context, span *observability2.ExternalCacheGetSpan) (context.Context, observability2.SpanHook) {
	return h.newSpan(ctx, span, "external_cache_get", targetSpanAttr(span.Target()))
}
