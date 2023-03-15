package summary

import (
	"context"
	"heph/engine/observability"
	log "heph/hlog"
	"sync"
)

type Summary struct {
	observability.BaseHook
	Spans    map[string]*TargetStats
	spansm   sync.Mutex
	RootSpan *observability.BaseSpan
}

func (s *Summary) Reset() {
	s.spansm.Lock()
	defer s.spansm.Unlock()

	s.Spans = map[string]*TargetStats{}
}

func prepare[S interface {
	observability.SpanTarget
	observability.SpanError
}](s *Summary, ctx context.Context, span S) (*TargetStats, *TargetStatsSpan) {
	fqn := span.Target().FQN

	s.spansm.Lock()
	defer s.spansm.Unlock()

	if s.Spans == nil {
		s.Spans = map[string]*TargetStats{}
	}

	stat, ok := s.Spans[fqn]
	if !ok {
		stat = &TargetStats{
			FQN: fqn,
		}
		s.Spans[fqn] = stat
	}

	tstat := &TargetStatsSpan{}
	tstat.Start = span.StartTime()
	tstat.End = span.EndTime()
	if span.Error() != nil {
		tstat.Error = true
	}
	stat.Gen = observability.IsDuringGen(ctx)

	return stat, tstat
}

func prepareArtifact[S interface {
	observability.SpanTargetArtifact
	observability.SpanError
}](s *Summary, ctx context.Context, span S) (*TargetStats, TargetStatsArtifact) {
	ts, tstat := prepare(s, ctx, span)

	astats := TargetStatsArtifact{
		Name:        span.Artifact().Name(),
		DisplayName: span.Artifact().DisplayName(),
		Start:       tstat.Start,
		End:         tstat.End,
	}
	if span.Error() != nil {
		astats.Error = true
	}

	return ts, astats
}

func prepareArtifactCache[S interface {
	observability.SpanTargetArtifact
	observability.SpanError
	observability.SpanCacheHit
}](s *Summary, ctx context.Context, span S) (*TargetStats, TargetStatsArtifact) {
	ts, astats := prepareArtifact(s, ctx, span)

	astats.CacheHit = span.IsCacheHit()

	return ts, astats
}

func (s *Summary) OnRoot(ctx context.Context, span *observability.BaseSpan) (context.Context, observability.Finalizer) {
	if s.RootSpan != nil {
		log.Warnf("rootspan is already defined")
	}

	s.RootSpan = span
	return ctx, nil
}

func (s *Summary) OnRun(ctx context.Context, span *observability.TargetSpan) (context.Context, observability.Finalizer) {
	return ctx, func() {
		// This should be called so that the TargetSpan has start & end time set properly
		_, _ = prepare(s, ctx, span)
	}
}

func (s *Summary) OnCacheDownload(ctx context.Context, span *observability.TargetArtifactCacheSpan) (context.Context, observability.Finalizer) {
	return ctx, func() {
		ts, tas := prepareArtifactCache(s, ctx, span)
		if ts == nil {
			return
		}
		ts.ArtifactsDownload = append(ts.ArtifactsDownload, tas)
	}
}

func (s *Summary) OnCacheUpload(ctx context.Context, span *observability.TargetArtifactSpan) (context.Context, observability.Finalizer) {
	return ctx, func() {
		ts, tas := prepareArtifact(s, ctx, span)
		if ts == nil {
			return
		}
		ts.ArtifactsUpload = append(ts.ArtifactsUpload, tas)
	}
}

func (s *Summary) OnRunPrepare(ctx context.Context, span *observability.TargetSpan) (context.Context, observability.Finalizer) {
	return ctx, func() {
		ts, tss := prepare(s, ctx, span)
		ts.Prepare = tss
	}
}

func (s *Summary) OnRunExec(ctx context.Context, span *observability.TargetSpan) (context.Context, observability.Finalizer) {
	return ctx, func() {
		ts, tss := prepare(s, ctx, span)
		ts.Exec = tss
	}
}

func (s *Summary) OnCollectOutput(ctx context.Context, span *observability.TargetSpan) (context.Context, observability.Finalizer) {
	return ctx, func() {
		ts, tss := prepare(s, ctx, span)
		ts.CollectOutput = tss
	}
}

func (s *Summary) OnLocalCacheStore(ctx context.Context, span *observability.TargetSpan) (context.Context, observability.Finalizer) {
	return ctx, func() {
		ts, tss := prepare(s, ctx, span)
		ts.CacheStore = tss
	}
}

func (s *Summary) OnLocalCacheCheck(ctx context.Context, span *observability.TargetArtifactCacheSpan) (context.Context, observability.Finalizer) {
	return ctx, func() {
		ts, tas := prepareArtifactCache(s, ctx, span)
		if ts == nil {
			return
		}
		ts.ArtifactsLocalGet = append(ts.ArtifactsLocalGet, tas)
	}
}
