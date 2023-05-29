package observability

import (
	"context"
	"github.com/google/uuid"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/tgt"
	"io"
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

func newSpan[S spanInternal](ctx context.Context, hooks []Hook, span S, fun func(Hook, context.Context, S) (context.Context, SpanHook)) (context.Context, S) {
	span.setId(nextID())
	span.SetStartTime(time.Now())

	for _, hook := range hooks {
		var shook SpanHook
		ctx, shook = fun(hook, ctx, span)
		span.registerHook(shook)
	}

	return ctx, span
}

type spanInternalWithId interface {
	spanInternal
	spanDedupId() interface{}
}

func newSpanWithDedup[S spanInternalWithId](hookName string, ctx context.Context, hooks []Hook, span S, fun func(Hook, context.Context, S) (context.Context, SpanHook)) (context.Context, S) {
	keyCtx := struct {
		hookname string
		sid      interface{}
	}{
		hookname: hookName,
		sid:      span.spanDedupId(),
	}

	if span, ok := ctx.Value(keyCtx).(S); ok {
		return ctx, span
	}

	ctx, nspan := newSpan[S](ctx, hooks, span, fun)

	ctx = context.WithValue(ctx, keyCtx, nspan)

	return ctx, nspan
}

func (t *Observability) SpanRoot(ctx context.Context) (context.Context, *BaseSpan) {
	span := &BaseSpan{}
	return newSpan(ctx, t.hooks, span, Hook.OnRoot)
}

func (t *Observability) SpanRun(ctx context.Context, target *tgt.Target) (context.Context, *TargetSpan) {
	span := &TargetSpan{target: target}
	return newSpanWithDedup("Run", ctx, t.hooks, span, Hook.OnRun)
}

func (t *Observability) SpanGenPass(ctx context.Context) (context.Context, *BaseSpan) {
	span := &BaseSpan{}
	ctx, span = newSpan(ctx, t.hooks, span, Hook.OnGenPass)

	ctx = context.WithValue(ctx, duringGenKey{}, true)

	return ctx, span
}

func (t *Observability) SpanCacheDownload(ctx context.Context, target *tgt.Target, cache string, artifact artifacts.Artifact) (context.Context, *TargetArtifactCacheSpan) {
	span := &TargetArtifactCacheSpan{TargetArtifactSpan: TargetArtifactSpan{TargetSpan: TargetSpan{target: target}, artifact: artifact}, Cache: cache}
	return newSpanWithDedup("CacheDownload", ctx, t.hooks, span, Hook.OnCacheDownload)
}

func (t *Observability) SpanCacheUpload(ctx context.Context, target *tgt.Target, cache string, artifact artifacts.Artifact) (context.Context, *TargetArtifactCacheSpan) {
	span := &TargetArtifactCacheSpan{TargetArtifactSpan: TargetArtifactSpan{TargetSpan: TargetSpan{target: target}, artifact: artifact}, Cache: cache}
	return newSpanWithDedup("CacheUpload", ctx, t.hooks, span, Hook.OnCacheUpload)
}

func (t *Observability) SpanRunPrepare(ctx context.Context, target *tgt.Target) (context.Context, *TargetSpan) {
	span := &TargetSpan{target: target}
	return newSpanWithDedup("RunPrepare", ctx, t.hooks, span, Hook.OnRunPrepare)
}

func (t *Observability) SpanRunExec(ctx context.Context, target *tgt.Target) (context.Context, *TargetExecSpan) {
	span := &TargetExecSpan{TargetSpan: TargetSpan{target: target}, ExecId: uuid.New()}
	return newSpanWithDedup("RunExec", ctx, t.hooks, span, Hook.OnRunExec)
}

func (t *Observability) SpanCollectOutput(ctx context.Context, target *tgt.Target) (context.Context, *TargetSpan) {
	span := &TargetSpan{target: target}
	return newSpanWithDedup("CollectOutput", ctx, t.hooks, span, Hook.OnCollectOutput)
}

func (t *Observability) SpanLocalCacheStore(ctx context.Context, target *tgt.Target) (context.Context, *TargetSpan) {
	span := &TargetSpan{target: target}
	return newSpanWithDedup("LocalCacheStore", ctx, t.hooks, span, Hook.OnLocalCacheStore)
}

func (t *Observability) SpanLocalCacheCheck(ctx context.Context, target *tgt.Target, artifact artifacts.Artifact) (context.Context, *TargetArtifactCacheSpan) {
	span := &TargetArtifactCacheSpan{TargetArtifactSpan: TargetArtifactSpan{TargetSpan: TargetSpan{target: target}, artifact: artifact}}
	return newSpanWithDedup("LocalCacheCheck", ctx, t.hooks, span, Hook.OnLocalCacheCheck)
}

func (t *Observability) SpanExternalCacheGet(ctx context.Context, target *tgt.Target, cache string, outputs []string, onlyMeta bool) (context.Context, *ExternalCacheGetSpan) {
	span := &ExternalCacheGetSpan{TargetSpan: TargetSpan{target: target}, Cache: cache, Outputs: outputs, OnlyMeta: onlyMeta}
	return newSpanWithDedup("ExternalCacheGet", ctx, t.hooks, span, Hook.OnExternalCacheGet)
}

func (t *Observability) LogsWriter(ctx context.Context) io.Writer {
	var ws []io.Writer

	for _, hook := range t.hooks {
		w := hook.OnLogs(ctx)
		if w != nil {
			ws = append(ws, w)
		}
	}

	if len(ws) == 0 {
		return nil
	}

	return io.MultiWriter(ws...)
}
