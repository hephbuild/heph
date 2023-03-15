package observability

import (
	"context"
	"heph/engine/artifacts"
	"heph/tgt"
	"time"
)

type Observability struct {
	hooks []Hook
}

func NewTelemetry() *Observability {
	t := &Observability{}

	return t
}

func (t *Observability) RegisterHook(h Hook) {
	t.hooks = append(t.hooks, h)
}

func newSpan[S spanInternal](ctx context.Context, hooks []Hook, span S, fun func(Hook, context.Context, S) (context.Context, Finalizer)) (context.Context, S) {
	span.setStart(time.Now())

	for _, hook := range hooks {
		var finalizer Finalizer
		ctx, finalizer = fun(hook, ctx, span)
		span.registerFinalizer(finalizer)
	}

	return ctx, span
}

func (t *Observability) SpanRoot(ctx context.Context) (context.Context, *BaseSpan) {
	span := &BaseSpan{}
	return newSpan(ctx, t.hooks, span, Hook.OnRoot)
}

func (t *Observability) SpanRun(ctx context.Context, target *tgt.Target) (context.Context, *TargetSpan) {
	span := &TargetSpan{target: target}
	return newSpan(ctx, t.hooks, span, Hook.OnRun)
}

func (t *Observability) SpanGenPass(ctx context.Context) (context.Context, *BaseSpan) {
	span := &BaseSpan{}
	ctx, span = newSpan(ctx, t.hooks, span, Hook.OnGenPass)

	ctx = context.WithValue(ctx, duringGenKey{}, true)

	return ctx, span
}

func (t *Observability) SpanCacheDownload(ctx context.Context, target *tgt.Target, artifact artifacts.Artifact) (context.Context, *TargetArtifactCacheSpan) {
	span := &TargetArtifactCacheSpan{TargetArtifactSpan: TargetArtifactSpan{TargetSpan: TargetSpan{target: target}, artifact: artifact}}
	return newSpan(ctx, t.hooks, span, Hook.OnCacheDownload)
}

func (t *Observability) SpanCacheUpload(ctx context.Context, target *tgt.Target, artifact artifacts.Artifact) (context.Context, *TargetArtifactSpan) {
	span := &TargetArtifactSpan{TargetSpan: TargetSpan{target: target}, artifact: artifact}
	return newSpan(ctx, t.hooks, span, Hook.OnCacheUpload)
}

func (t *Observability) SpanRunPrepare(ctx context.Context, target *tgt.Target) (context.Context, *TargetSpan) {
	span := &TargetSpan{target: target}
	return newSpan(ctx, t.hooks, span, Hook.OnRunPrepare)
}

func (t *Observability) SpanRunExec(ctx context.Context, target *tgt.Target) (context.Context, *TargetSpan) {
	span := &TargetSpan{target: target}
	return newSpan(ctx, t.hooks, span, Hook.OnRunExec)
}

func (t *Observability) SpanCollectOutput(ctx context.Context, target *tgt.Target) (context.Context, *TargetSpan) {
	span := &TargetSpan{target: target}
	return newSpan(ctx, t.hooks, span, Hook.OnCollectOutput)
}

func (t *Observability) SpanLocalCacheStore(ctx context.Context, target *tgt.Target) (context.Context, *TargetSpan) {
	span := &TargetSpan{target: target}
	return newSpan(ctx, t.hooks, span, Hook.OnLocalCacheStore)
}

func (t *Observability) SpanLocalCacheCheck(ctx context.Context, target *tgt.Target, artifact artifacts.Artifact) (context.Context, *TargetArtifactCacheSpan) {
	span := &TargetArtifactCacheSpan{TargetArtifactSpan: TargetArtifactSpan{TargetSpan: TargetSpan{target: target}, artifact: artifact}}
	return newSpan(ctx, t.hooks, span, Hook.OnLocalCacheCheck)
}

func (t *Observability) SpanExternalCacheGet(ctx context.Context, target *tgt.Target, cache string, outputs []string, onlyMeta bool) (context.Context, *ExternalCacheGetSpan) {
	span := &ExternalCacheGetSpan{TargetSpan: TargetSpan{target: target}, Cache: cache, Outputs: outputs, OnlyMeta: onlyMeta}
	return newSpan(ctx, t.hooks, span, Hook.OnExternalCacheGet)
}
