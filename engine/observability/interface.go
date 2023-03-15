package observability

import (
	"context"
	"heph/engine/artifacts"
	"heph/tgt"
	"time"
)

type Span interface {
	End()
	StartTime() time.Time
	EndTime() time.Time
}

type spanInternal interface {
	registerFinalizer(Finalizer)
	setStart(time.Time)
}

type SpanError interface {
	Span
	EndError(err error)
	Error() error
}

type SpanTarget interface {
	Span
	Target() *tgt.Target
}

type SpanTargetArtifact interface {
	SpanTarget
	Artifact() artifacts.Artifact
}

type SpanCacheHit interface {
	IsCacheHit() bool
}

type Finalizer func()

type BaseSpan struct {
	err        error
	finalized  bool
	finalizers []Finalizer
	start      time.Time
	end        time.Time
}

func (s *BaseSpan) StartTime() time.Time {
	return s.start
}

func (s *BaseSpan) EndTime() time.Time {
	return s.end
}

func (s *BaseSpan) Error() error {
	return s.err
}

func (s *BaseSpan) EndError(err error) {
	if s.finalized {
		return
	}

	s.err = err
	s.End()
}

func (s *BaseSpan) End() {
	if s.finalized {
		return
	}

	s.finalized = true
	s.end = time.Now()

	for _, finalizer := range s.finalizers {
		finalizer()
	}
}

func (s *BaseSpan) registerFinalizer(finalizer Finalizer) {
	if finalizer != nil {
		s.finalizers = append(s.finalizers, finalizer)
	}
}

func (s *BaseSpan) setStart(start time.Time) {
	s.start = start
}

type TargetSpan struct {
	BaseSpan
	target *tgt.Target
}

func (s *TargetSpan) Target() *tgt.Target {
	return s.target
}

type TargetArtifactSpan struct {
	TargetSpan
	artifact artifacts.Artifact
}

func (s *TargetArtifactSpan) Artifact() artifacts.Artifact {
	return s.artifact
}

type TargetArtifactCacheSpan struct {
	TargetArtifactSpan
	cacheHit bool
}

func (s *TargetArtifactCacheSpan) SetCacheHit(v bool) {
	s.cacheHit = v
}

func (s *TargetArtifactCacheSpan) IsCacheHit() bool {
	return s.cacheHit
}

type TargetsSpan struct {
	BaseSpan
	Targets []*tgt.Target
}

type ExternalCacheGetSpan struct {
	TargetSpan
	Cache    string
	Outputs  []string
	OnlyMeta bool
}

type Hook interface {
	OnRoot(ctx context.Context, span *BaseSpan) (context.Context, Finalizer)
	OnRun(ctx context.Context, span *TargetSpan) (context.Context, Finalizer)
	OnGenPass(ctx context.Context, span *BaseSpan) (context.Context, Finalizer)
	OnCacheDownload(ctx context.Context, span *TargetArtifactCacheSpan) (context.Context, Finalizer)
	OnCacheUpload(ctx context.Context, span *TargetArtifactSpan) (context.Context, Finalizer)
	OnRunPrepare(ctx context.Context, span *TargetSpan) (context.Context, Finalizer)
	OnRunExec(ctx context.Context, span *TargetSpan) (context.Context, Finalizer)
	OnCollectOutput(ctx context.Context, span *TargetSpan) (context.Context, Finalizer)
	OnLocalCacheStore(ctx context.Context, span *TargetSpan) (context.Context, Finalizer)
	OnLocalCacheCheck(ctx context.Context, span *TargetArtifactCacheSpan) (context.Context, Finalizer)
	OnExternalCacheGet(ctx context.Context, span *ExternalCacheGetSpan) (context.Context, Finalizer)
}

type BaseHook struct{}

var _ Hook = (*BaseHook)(nil)

func (b BaseHook) OnRoot(ctx context.Context, span *BaseSpan) (context.Context, Finalizer) {
	return ctx, nil
}
func (b BaseHook) OnRun(ctx context.Context, _ *TargetSpan) (context.Context, Finalizer) {
	return ctx, nil
}
func (b BaseHook) OnGenPass(ctx context.Context, _ *BaseSpan) (context.Context, Finalizer) {
	return ctx, nil
}
func (b BaseHook) OnCacheDownload(ctx context.Context, _ *TargetArtifactCacheSpan) (context.Context, Finalizer) {
	return ctx, nil
}
func (b BaseHook) OnCacheUpload(ctx context.Context, _ *TargetArtifactSpan) (context.Context, Finalizer) {
	return ctx, nil
}
func (b BaseHook) OnRunPrepare(ctx context.Context, _ *TargetSpan) (context.Context, Finalizer) {
	return ctx, nil
}
func (b BaseHook) OnRunExec(ctx context.Context, _ *TargetSpan) (context.Context, Finalizer) {
	return ctx, nil
}
func (b BaseHook) OnCollectOutput(ctx context.Context, _ *TargetSpan) (context.Context, Finalizer) {
	return ctx, nil
}
func (b BaseHook) OnLocalCacheStore(ctx context.Context, _ *TargetSpan) (context.Context, Finalizer) {
	return ctx, nil
}
func (b BaseHook) OnLocalCacheCheck(ctx context.Context, _ *TargetArtifactCacheSpan) (context.Context, Finalizer) {
	return ctx, nil
}
func (b BaseHook) OnExternalCacheGet(ctx context.Context, _ *ExternalCacheGetSpan) (context.Context, Finalizer) {
	return ctx, nil
}
