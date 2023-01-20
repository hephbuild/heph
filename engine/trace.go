package engine

import (
	"context"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/codes"
	"go.opentelemetry.io/otel/trace"
	"heph/engine/htrace"
	"os"
	"strings"
)

func targetSpanAttr(t *Target) trace.SpanStartOption {
	attrs := []attribute.KeyValue{
		{
			Key:   htrace.AttrTargetAddr,
			Value: attribute.StringValue(t.FQN),
		},
	}

	return trace.WithAttributes(attrs...)
}

type Span struct {
	trace.Span
}

func (s Span) EndError(err error) {
	if err != nil {
		s.Span.RecordError(err)
		s.Span.SetStatus(codes.Error, "")
	}
	s.Span.End()
}

func (e *Engine) newTargetSpanPure(ctx context.Context, spanName, phase string, opts ...trace.SpanStartOption) Span {
	_, span := e.newTargetSpan(ctx, spanName, phase, opts...)
	return span
}

func (e *Engine) newTargetSpan(ctx context.Context, spanName, phase string, opts ...trace.SpanStartOption) (context.Context, Span) {
	if spanName == "" {
		spanName = phase
	}

	if ctx == nil {
		ctx = context.Background()
	}

	if !trace.SpanFromContext(ctx).SpanContext().IsValid() {
		ctx = trace.ContextWithSpan(ctx, e.RootSpan)
	}

	attrs := []attribute.KeyValue{
		{
			Key:   htrace.AttrPhase,
			Value: attribute.StringValue(phase),
		},
	}

	opts = append(opts, trace.WithAttributes(attrs...))

	ctx, span := e.Tracer.Start(ctx, spanName, opts...)

	return ctx, Span{span}
}

func (e *Engine) StartRootSpan() {
	if e.RootSpan != nil {
		panic("root span already started")
	}

	args := append([]string{"heph"}, os.Args[1:]...)
	attrs := []attribute.KeyValue{
		{
			Key:   "heph.args",
			Value: attribute.StringSliceValue(args),
		},
		{
			Key:   htrace.AttrRoot,
			Value: attribute.BoolValue(true),
		},
	}

	_, e.RootSpan = e.Tracer.Start(context.Background(), strings.Join(args, " "), trace.WithAttributes(attrs...))
}

func (e *Engine) SpanRun(ctx context.Context, t *Target) (context.Context, Span) {
	return e.newTargetSpan(ctx, "run "+t.FQN, htrace.PhaseTargetRun, targetSpanAttr(t))
}

func (e *Engine) SpanGenPass(ctx context.Context) (context.Context, Span) {
	ctx, span := e.newTargetSpan(ctx, "", htrace.PhaseGenPass)

	return context.WithValue(ctx, htrace.AttrDuringGen, true), span
}

func (e *Engine) SpanScheduleTargetWithDeps(ctx context.Context, targets []*Target) (context.Context, Span) {
	fqns := make([]string, 0)
	for _, target := range targets {
		fqns = append(fqns, target.FQN)
	}
	attrs := []attribute.KeyValue{
		{
			Key:   "heph.targets",
			Value: attribute.StringSliceValue(fqns),
		},
	}

	return e.newTargetSpan(ctx, "", htrace.PhaseScheduleTargetWithDeps, trace.WithAttributes(attrs...))
}

func (e *Engine) SpanCachePull(ctx context.Context, t *Target, output string, onlyMeta bool) Span {
	phase := htrace.PhaseCachePull
	if onlyMeta {
		phase = htrace.PhaseCachePullMeta
	}
	return e.newTargetSpanPure(ctx, phase, phase, targetSpanAttr(t), trace.WithAttributes(attribute.String(htrace.AttrOutput, output)))
}

func (e *Engine) SpanRunPrepare(ctx context.Context, t *Target) Span {
	return e.newTargetSpanPure(ctx, "", htrace.PhaseRunPrepare, targetSpanAttr(t))
}

func (e *Engine) SpanRunExec(ctx context.Context, t *Target) Span {
	return e.newTargetSpanPure(ctx, "", htrace.PhaseRunExec, targetSpanAttr(t))
}

func (e *Engine) SpanCollectOutput(ctx context.Context, t *Target) Span {
	return e.newTargetSpanPure(ctx, "", htrace.PhaseRunCollectOutput, targetSpanAttr(t))
}

func (e *Engine) SpanCacheStore(ctx context.Context, t *Target) Span {
	return e.newTargetSpanPure(ctx, "", htrace.PhaseRunCacheStore, targetSpanAttr(t))
}
