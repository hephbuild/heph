package observability

import (
	"context"
	"github.com/google/uuid"
	"github.com/hephbuild/heph/artifacts"
	"github.com/hephbuild/heph/graph"
	"io"
	"sync"
	"sync/atomic"
	"time"
)

type Span interface {
	ID() uint64
	End()
	ScheduledTime() time.Time
	QueuedTime() time.Time
	StartTime() time.Time
	EndTime() time.Time

	SetScheduledTime(time.Time)
	SetQueuedTime(time.Time)
	SetStartTime(time.Time)
}

type spanInternal interface {
	setId(id uint64)
	registerHook(SpanHook)
	SetStartTime(time.Time)
}

type SpanError interface {
	Span
	EndError(err error)
	EndErrorState(err error, state State)
	FinalState() State
	Error() error
}

type SpanTarget interface {
	Span
	Target() *graph.Target
}

type SpanTargetArtifact interface {
	SpanTarget
	Artifact() artifacts.Artifact
}

type SpanCacheHit interface {
	IsCacheHit() *bool
}

type SpanHook interface {
	OnSchedule()
	OnQueued()
	OnStart()
	OnEnd()
}

type SpanHookFunc struct {
	OnScheduleFunc func()
	OnQueuedFunc   func()
	OnStartFunc    func()
	OnEndFunc      func()
}

func (s SpanHookFunc) OnSchedule() {
	if s.OnScheduleFunc != nil {
		s.OnScheduleFunc()
	}
}

func (s SpanHookFunc) OnQueued() {
	if s.OnQueuedFunc != nil {
		s.OnQueuedFunc()
	}
}

func (s SpanHookFunc) OnStart() {
	if s.OnStartFunc != nil {
		s.OnStartFunc()
	}
}

func (s SpanHookFunc) OnEnd() {
	if s.OnEndFunc != nil {
		s.OnEndFunc()
	}
}

func FinalizerSpanHook(f func()) SpanHook {
	return SpanHookFunc{
		OnEndFunc: f,
	}
}

func AllSpanHook(f func()) SpanHook {
	return SpanHookFunc{
		OnScheduleFunc: f,
		OnQueuedFunc:   f,
		OnStartFunc:    f,
		OnEndFunc:      f,
	}
}

type State int8

const (
	StateUnknown State = iota
	StateSuccess
	StateFailed
	StateSkipped
)

type BaseSpan struct {
	id            uint64
	err           error
	finalized     bool
	hooks         []SpanHook
	scheduledTime time.Time
	queuedTime    time.Time
	startTime     time.Time
	endTime       time.Time
	state         State

	m sync.Mutex
}

var idc uint64

func nextID() uint64 {
	return atomic.AddUint64(&idc, 1)
}

func (s *BaseSpan) setId(id uint64) {
	s.id = id
}

func (s *BaseSpan) ID() uint64 {
	return s.id
}

func (s *BaseSpan) ScheduledTime() time.Time {
	return s.scheduledTime
}
func (s *BaseSpan) QueuedTime() time.Time {
	return s.queuedTime
}
func (s *BaseSpan) StartTime() time.Time {
	return s.startTime
}
func (s *BaseSpan) EndTime() time.Time {
	return s.endTime
}

func (s *BaseSpan) SetScheduledTime(t time.Time) {
	s.scheduledTime = t
	s.runHooks(SpanHook.OnSchedule)
}
func (s *BaseSpan) SetQueuedTime(t time.Time) {
	s.queuedTime = t
	s.runHooks(SpanHook.OnQueued)
}
func (s *BaseSpan) SetStartTime(t time.Time) {
	s.startTime = t
	s.runHooks(SpanHook.OnStart)
}

func (s *BaseSpan) Error() error {
	return s.err
}

func (s *BaseSpan) EndError(err error) {
	if err == nil {
		s.End()
	} else {
		s.EndErrorState(err, StateFailed)
	}
}

func (s *BaseSpan) EndErrorState(err error, state State) {
	s.m.Lock()
	defer s.m.Unlock()

	if s.finalized {
		return
	}

	s.finalized = true

	s.err = err
	s.state = state
	s.endTime = time.Now()

	s.runHooks(SpanHook.OnEnd)
}

func (s *BaseSpan) FinalState() State {
	return s.state
}

func (s *BaseSpan) End() {
	s.EndErrorState(nil, StateSuccess)
}

func (s *BaseSpan) runHooks(h func(SpanHook)) {
	for _, hook := range s.hooks {
		h(hook)
	}
}

func (s *BaseSpan) registerHook(hook SpanHook) {
	if hook != nil {
		s.hooks = append(s.hooks, hook)
	}
}

type TargetSpan struct {
	BaseSpan
	target *graph.Target
}

func (s *TargetSpan) spanDedupId() interface{} {
	return s.target.Addr
}

func (s *TargetSpan) Target() *graph.Target {
	return s.target
}

type TargetExecSpan struct {
	TargetSpan
	ExecId uuid.UUID
}

func (s *TargetExecSpan) spanDedupId() interface{} {
	return struct {
		base   interface{}
		execId uuid.UUID
	}{
		base:   s.TargetSpan.spanDedupId(),
		execId: s.ExecId,
	}
}

type TargetArtifactSpan struct {
	TargetSpan
	artifact artifacts.Artifact
}

func (s *TargetArtifactSpan) spanDedupId() interface{} {
	return struct {
		base     interface{}
		artifact string
	}{
		base:     s.TargetSpan.spanDedupId(),
		artifact: s.artifact.Name(),
	}
}

func (s *TargetArtifactSpan) Artifact() artifacts.Artifact {
	return s.artifact
}

type TargetArtifactCacheSpan struct {
	TargetArtifactSpan
	Cache    string
	cacheHit *bool
}

func (s *TargetArtifactCacheSpan) spanDedupId() interface{} {
	return struct {
		base  interface{}
		cache string
	}{
		base:  s.TargetArtifactSpan.spanDedupId(),
		cache: s.Cache,
	}
}

func (s *TargetArtifactCacheSpan) SetCacheHit(v bool) {
	s.cacheHit = &v
}

func (s *TargetArtifactCacheSpan) IsCacheHit() *bool {
	return s.cacheHit
}

type ExternalCacheGetSpan struct {
	TargetSpan
	Cache    string
	Outputs  []string
	OnlyMeta bool
	cacheHit *bool
}

func (s *ExternalCacheGetSpan) SetCacheHit(v bool) {
	s.cacheHit = &v
}

func (s *ExternalCacheGetSpan) IsCacheHit() *bool {
	return s.cacheHit
}

type Hook interface {
	OnRoot(ctx context.Context, span *BaseSpan) (context.Context, SpanHook)
	OnRun(ctx context.Context, span *TargetSpan) (context.Context, SpanHook)
	OnGenPass(ctx context.Context, span *BaseSpan) (context.Context, SpanHook)
	OnCacheDownload(ctx context.Context, span *TargetArtifactCacheSpan) (context.Context, SpanHook)
	OnCacheUpload(ctx context.Context, span *TargetArtifactCacheSpan) (context.Context, SpanHook)
	OnRunPrepare(ctx context.Context, span *TargetSpan) (context.Context, SpanHook)
	OnRunExec(ctx context.Context, span *TargetExecSpan) (context.Context, SpanHook)
	OnCollectOutput(ctx context.Context, span *TargetSpan) (context.Context, SpanHook)
	OnLocalCacheStore(ctx context.Context, span *TargetSpan) (context.Context, SpanHook)
	OnLocalCacheCheck(ctx context.Context, span *TargetArtifactCacheSpan) (context.Context, SpanHook)
	OnExternalCacheGet(ctx context.Context, span *ExternalCacheGetSpan) (context.Context, SpanHook)

	OnLogs(ctx context.Context) io.Writer
}

type BaseHook struct{}

var _ Hook = (*BaseHook)(nil)

func (b BaseHook) OnRoot(ctx context.Context, _ *BaseSpan) (context.Context, SpanHook) {
	return ctx, nil
}
func (b BaseHook) OnRun(ctx context.Context, _ *TargetSpan) (context.Context, SpanHook) {
	return ctx, nil
}
func (b BaseHook) OnGenPass(ctx context.Context, _ *BaseSpan) (context.Context, SpanHook) {
	return ctx, nil
}
func (b BaseHook) OnCacheDownload(ctx context.Context, _ *TargetArtifactCacheSpan) (context.Context, SpanHook) {
	return ctx, nil
}
func (b BaseHook) OnCacheUpload(ctx context.Context, _ *TargetArtifactCacheSpan) (context.Context, SpanHook) {
	return ctx, nil
}
func (b BaseHook) OnRunPrepare(ctx context.Context, _ *TargetSpan) (context.Context, SpanHook) {
	return ctx, nil
}
func (b BaseHook) OnRunExec(ctx context.Context, _ *TargetExecSpan) (context.Context, SpanHook) {
	return ctx, nil
}
func (b BaseHook) OnCollectOutput(ctx context.Context, _ *TargetSpan) (context.Context, SpanHook) {
	return ctx, nil
}
func (b BaseHook) OnLocalCacheStore(ctx context.Context, _ *TargetSpan) (context.Context, SpanHook) {
	return ctx, nil
}
func (b BaseHook) OnLocalCacheCheck(ctx context.Context, _ *TargetArtifactCacheSpan) (context.Context, SpanHook) {
	return ctx, nil
}
func (b BaseHook) OnExternalCacheGet(ctx context.Context, _ *ExternalCacheGetSpan) (context.Context, SpanHook) {
	return ctx, nil
}

func (b BaseHook) OnLogs(ctx context.Context) io.Writer {
	return nil
}

func DoVE[T any](span SpanError, f func() (T, error)) (_ T, rerr error) {
	defer span.EndError(rerr)

	return f()
}

func DoE(span SpanError, f func() error) (rerr error) {
	defer span.EndError(rerr)

	return f()
}
